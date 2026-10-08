//! Transport to the providers through the in-process OAGW proxy.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use super::resolver::ResolvedProvider;
use super::sse_parser::SseParser;
use super::{Adapter, CompletionResult, LlmRequest, ProviderEvent, adapter_for, parse_error_payload};
use crate::domain::sanitize::sanitize_provider_message;

/// Narrow proxy port (production: OAGW; tests: scripted fake).
#[async_trait]
pub trait ProxyClient: Send + Sync {
    /// Send one request `/{alias}/...` and return the upstream response.
    ///
    /// # Errors
    /// Gateway-side failures (`DeadlineExceeded` on timeout).
    async fn proxy(&self, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError>;
}

/// S2S security context obtained at gear start.
#[derive(Default)]
pub struct S2sContext {
    ctx: RwLock<Option<SecurityContext>>,
}

impl S2sContext {
    pub fn set(&self, ctx: SecurityContext) {
        if let Ok(mut g) = self.ctx.write() {
            *g = Some(ctx);
        }
    }

    #[must_use]
    pub fn get(&self) -> Option<SecurityContext> {
        self.ctx.read().ok().and_then(|g| g.clone())
    }
}

/// Production proxy over `ServiceGatewayClientV1`, using the S2S context.
pub struct OagwProxy {
    gateway: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
}

impl OagwProxy {
    #[must_use]
    pub fn new(gateway: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self { gateway, s2s }
    }
}

#[async_trait]
impl ProxyClient for OagwProxy {
    async fn proxy(&self, req: http::Request<Body>) -> Result<http::Response<Body>, CanonicalError> {
        let Some(ctx) = self.s2s.get() else {
            return Err(CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("provider connectivity is not ready")
                .create());
        };
        self.gateway.proxy_request(ctx, req).await
    }
}

/// Provider failure before or during a stream.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("provider timeout")]
    Timeout,
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("provider error: {0}")]
    Provider(String),
}

impl ProviderError {
    /// Streaming error code (DESIGN §3.3 "Streaming error codes").
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Timeout => "provider_timeout",
            Self::RateLimited(_) => "rate_limited",
            Self::Provider(_) => "provider_error",
        }
    }

    /// Sanitized client message.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Timeout => "Provider request timed out".to_owned(),
            Self::RateLimited(m) | Self::Provider(m) => sanitize_provider_message(m),
        }
    }
}

fn map_gateway_err(e: &CanonicalError) -> ProviderError {
    match e {
        CanonicalError::DeadlineExceeded { .. } => ProviderError::Timeout,
        other => {
            tracing::warn!(error = %other, "provider gateway error");
            ProviderError::Provider("Provider is currently unavailable".to_owned())
        }
    }
}

const MAX_ERROR_BODY: usize = 64 * 1024;

async fn read_body_limited(body: Body) -> Bytes {
    match body {
        Body::Empty => Bytes::new(),
        Body::Bytes(b) => b,
        Body::Stream(mut s) => {
            let mut buf = Vec::new();
            while let Some(chunk) = s.next().await {
                match chunk {
                    Ok(c) => {
                        buf.extend_from_slice(&c);
                        if buf.len() > MAX_ERROR_BODY {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            Bytes::from(buf)
        }
    }
}

/// Map a non-2xx provider response to a provider error.
async fn error_from_response(resp: http::Response<Body>) -> ProviderError {
    let status = resp.status();
    let gateway = resp.extensions().get::<oagw_sdk::api::ErrorSource>()
        == Some(&oagw_sdk::api::ErrorSource::Gateway)
        || resp
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("gateway"));
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = read_body_limited(resp.into_body()).await;
    let text = String::from_utf8_lossy(&body).into_owned();
    if gateway && status == http::StatusCode::GATEWAY_TIMEOUT {
        return ProviderError::Timeout;
    }
    if status == http::StatusCode::TOO_MANY_REQUESTS {
        let msg = match retry_after {
            Some(s) => format!("Provider rate limit exceeded; retry after {s} seconds"),
            None => "Provider rate limit exceeded".to_owned(),
        };
        return ProviderError::RateLimited(msg);
    }
    if gateway {
        tracing::warn!(status = %status, body = %text, "provider gateway error response");
        return ProviderError::Provider("Provider is currently unavailable".to_owned());
    }
    let (_, message) = parse_error_payload(&text);
    let message = if text.trim().is_empty() || message == text && serde_json::from_str::<Value>(&text).is_err() {
        format!("Provider returned HTTP {}", status.as_u16())
    } else {
        message
    };
    ProviderError::Provider(message)
}

/// Live provider stream: SSE bytes → provider events, one at a time.
pub struct ProviderStream {
    body: oagw_sdk::body::BodyStream,
    parser: SseParser,
    adapter: Box<dyn Adapter>,
    pending: std::collections::VecDeque<ProviderEvent>,
    finished: bool,
}

impl ProviderStream {
    /// Next event; `None` when the body ended (with or without a terminal event).
    pub async fn next_event(&mut self) -> Option<Result<ProviderEvent, ProviderError>> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(Ok(ev));
            }
            if self.finished {
                return None;
            }
            match self.body.next().await {
                Some(Ok(chunk)) => {
                    for sse in self.parser.feed(&chunk) {
                        self.pending.extend(self.adapter.translate(&sse));
                    }
                }
                Some(Err(e)) => {
                    self.finished = true;
                    tracing::warn!(error = %e, "provider stream failed");
                    return Some(Err(ProviderError::Provider(
                        "Provider stream was interrupted".to_owned(),
                    )));
                }
                None => {
                    self.finished = true;
                    if let Some(sse) = self.parser.finish() {
                        self.pending.extend(self.adapter.translate(&sse));
                    }
                }
            }
        }
    }
}

/// High-level LLM client.
#[derive(Clone)]
pub struct LlmClient {
    proxy: Arc<dyn ProxyClient>,
}

impl LlmClient {
    #[must_use]
    pub fn new(proxy: Arc<dyn ProxyClient>) -> Self {
        Self { proxy }
    }

    #[must_use]
    pub fn proxy(&self) -> &Arc<dyn ProxyClient> {
        &self.proxy
    }

    fn build_request(provider: &ResolvedProvider, req: &LlmRequest) -> Result<http::Request<Body>, ProviderError> {
        let adapter = adapter_for(provider.kind);
        let body = adapter.build_body(req);
        let bytes = serde_json::to_vec(&body)
            .map_err(|e| ProviderError::Provider(format!("request encoding: {e}")))?;
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(provider.chat_uri(&req.provider_model_id))
            .header(http::header::CONTENT_TYPE, "application/json");
        if req.stream {
            builder = builder.header(http::header::ACCEPT, "text/event-stream");
        }
        builder
            .body(Body::from(bytes))
            .map_err(|e| ProviderError::Provider(format!("request build: {e}")))
    }

    /// Open a streaming provider request.
    ///
    /// # Errors
    /// Gateway or HTTP-level provider failure.
    pub async fn stream(
        &self,
        provider: &ResolvedProvider,
        req: &LlmRequest,
    ) -> Result<ProviderStream, ProviderError> {
        let http_req = Self::build_request(provider, req)?;
        let resp = self
            .proxy
            .proxy(http_req)
            .await
            .map_err(|e| map_gateway_err(&e))?;
        if !resp.status().is_success() {
            return Err(error_from_response(resp).await);
        }
        let body = resp.into_body().into_stream();
        Ok(ProviderStream {
            body,
            parser: SseParser::new(),
            adapter: adapter_for(provider.kind),
            pending: std::collections::VecDeque::new(),
            finished: false,
        })
    }

    /// Non-streaming completion (thread summary).
    ///
    /// # Errors
    /// Gateway, HTTP or provider-reported failure.
    pub async fn complete(
        &self,
        provider: &ResolvedProvider,
        req: &LlmRequest,
        timeout: Duration,
    ) -> Result<CompletionResult, ProviderError> {
        let http_req = Self::build_request(provider, req)?;
        let fut = async {
            let resp = self
                .proxy
                .proxy(http_req)
                .await
                .map_err(|e| map_gateway_err(&e))?;
            if !resp.status().is_success() {
                return Err(error_from_response(resp).await);
            }
            let bytes = resp
                .into_body()
                .into_bytes()
                .await
                .map_err(|e| ProviderError::Provider(format!("response read: {e}")))?;
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| ProviderError::Provider(format!("invalid provider response: {e}")))?;
            adapter_for(provider.kind)
                .parse_completion(&v)
                .map_err(ProviderError::Provider)
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ProviderError::Timeout)?
    }
}
