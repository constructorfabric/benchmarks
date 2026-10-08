//! HTTP transport to LLM / RAG providers. Production: OAGW in-process proxy.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use oagw_sdk::{Body, MultipartBody, Part, ServiceGatewayClientV1, ServiceGatewayError};
use toolkit::client_hub::ClientHub;
use toolkit_security::SecurityContext;

/// One raw SSE event from the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Buffered (non-streaming) HTTP response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub retry_after_secs: Option<u64>,
    pub body: Bytes,
}

impl HttpResponse {
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Body parsed as JSON (`Value::Null` when not JSON).
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// Transport-level failure (no HTTP response from the provider).
#[derive(Debug, Clone, thiserror::Error)]
pub enum TransportError {
    /// Gateway or provider timeout (`provider_timeout`).
    #[error("timeout: {0}")]
    Timeout(String),
    /// Anything else (`provider_error`).
    #[error("transport error: {0}")]
    Other(String),
}

pub type RawEventStream = BoxStream<'static, Result<RawSseEvent, TransportError>>;

/// Result of a streaming POST.
pub enum StreamOutcome {
    /// Provider answered with an SSE stream.
    Events(RawEventStream),
    /// Provider answered with a non-SSE response (usually an error).
    Http(HttpResponse),
}

/// One multipart form field.
#[derive(Debug, Clone)]
pub struct MultipartField {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub data: Bytes,
}

/// Outgoing request body.
#[derive(Debug, Clone)]
pub enum OutgoingBody {
    Empty,
    Json(serde_json::Value),
    Multipart(Vec<MultipartField>),
}

/// Provider transport port. `uri` is `/{alias}{path}[?query]`.
#[async_trait]
pub trait ProviderTransport: Send + Sync {
    async fn request(
        &self,
        ctx: &SecurityContext,
        method: http::Method,
        uri: &str,
        body: OutgoingBody,
    ) -> Result<HttpResponse, TransportError>;

    async fn stream(
        &self,
        ctx: &SecurityContext,
        uri: &str,
        body: serde_json::Value,
    ) -> Result<StreamOutcome, TransportError>;
}

/// OAGW-backed transport. Provider calls use the gear's S2S context once provisioned.
pub struct OagwTransport {
    hub: Arc<ClientHub>,
    s2s: RwLock<Option<SecurityContext>>,
}

impl OagwTransport {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub, s2s: RwLock::new(None) }
    }

    pub fn set_s2s_context(&self, ctx: SecurityContext) {
        if let Ok(mut g) = self.s2s.write() {
            *g = Some(ctx);
        }
    }

    fn effective_ctx(&self, ctx: &SecurityContext) -> SecurityContext {
        self.s2s.read().ok().and_then(|g| g.clone()).unwrap_or_else(|| ctx.clone())
    }

    fn client(&self) -> Result<Arc<dyn ServiceGatewayClientV1>, TransportError> {
        self.hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| TransportError::Other(format!("oagw client unavailable: {e}")))
    }
}

fn map_gateway_err(err: toolkit_canonical_errors::CanonicalError) -> TransportError {
    match ServiceGatewayError::from(err) {
        ServiceGatewayError::Timeout => TransportError::Timeout("gateway timeout".into()),
        other => TransportError::Other(format!("gateway error: {other}")),
    }
}

fn retry_after(headers: &http::HeaderMap) -> Option<u64> {
    headers.get(http::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok())
}

async fn buffer(resp: http::Response<Body>) -> Result<HttpResponse, TransportError> {
    let status = resp.status().as_u16();
    let ra = retry_after(resp.headers());
    let body = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| TransportError::Other(format!("reading provider body failed: {e}")))?;
    // OAGW reports its own 504 as a deadline_exceeded Problem.
    if status == 504 && body.windows(17).any(|w| w == b"deadline_exceeded") {
        return Err(TransportError::Timeout("gateway deadline exceeded".into()));
    }
    Ok(HttpResponse { status, retry_after_secs: ra, body })
}

#[async_trait]
impl ProviderTransport for OagwTransport {
    async fn request(
        &self,
        ctx: &SecurityContext,
        method: http::Method,
        uri: &str,
        body: OutgoingBody,
    ) -> Result<HttpResponse, TransportError> {
        let gw = self.client()?;
        let req = match body {
            OutgoingBody::Empty => http::Request::builder().method(method).uri(uri).body(Body::Empty),
            OutgoingBody::Json(v) => http::Request::builder()
                .method(method)
                .uri(uri)
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&v).unwrap_or_default())),
            OutgoingBody::Multipart(fields) => {
                let mut mp = MultipartBody::new();
                for f in fields {
                    let mut part = Part::bytes(&f.name, f.data);
                    if let Some(name) = f.filename {
                        part = part.filename(name);
                    }
                    if let Some(ct) = f.content_type {
                        part = part.content_type(ct);
                    }
                    mp = mp.part(part);
                }
                mp.into_request(method, uri)
            }
        }
        .map_err(|e| TransportError::Other(format!("building request failed: {e}")))?;
        let resp = gw.proxy_request(self.effective_ctx(ctx), req).await.map_err(map_gateway_err)?;
        buffer(resp).await
    }

    async fn stream(
        &self,
        ctx: &SecurityContext,
        uri: &str,
        body: serde_json::Value,
    ) -> Result<StreamOutcome, TransportError> {
        let gw = self.client()?;
        let req = http::Request::builder()
            .method(http::Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "text/event-stream")
            .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
            .map_err(|e| TransportError::Other(format!("building request failed: {e}")))?;
        let resp = gw.proxy_request(self.effective_ctx(ctx), req).await.map_err(map_gateway_err)?;
        if !resp.status().is_success() {
            return Ok(StreamOutcome::Http(buffer(resp).await?));
        }
        match ServerEventsStream::from_response::<ServerEvent>(resp) {
            ServerEventsResponse::Events(events) => Ok(StreamOutcome::Events(
                events
                    .map(|r| {
                        r.map(|ev| RawSseEvent { event: ev.event, data: ev.data })
                            .map_err(|e| TransportError::Other(format!("provider stream error: {e}")))
                    })
                    .boxed(),
            )),
            ServerEventsResponse::Response(resp) => Ok(StreamOutcome::Http(buffer(resp).await?)),
        }
    }
}
