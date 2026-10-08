//! OAGW transport: sends provider requests through the in-process
//! `ServiceGatewayClientV1::proxy_request` with the gear's S2S context
//! (upstreams and routes are owned by that context's tenant).

use std::pin::Pin;
use std::sync::Arc;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{HeaderMap, Method, StatusCode};
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::Value;
use tokio::sync::RwLock;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use super::resolver::ChatTarget;
use super::sse::SseParser;
use super::{Adapter, LlmEvent, LlmFailure, LlmRequest, ParseState, Usage, adapter_for, error_message};

pub type EventStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

/// Lazily obtained S2S security context (client credentials exchange).
pub struct S2sContext {
    hub: Arc<ClientHub>,
    client_id: String,
    client_secret: String,
    ctx: RwLock<Option<SecurityContext>>,
}

impl S2sContext {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, client_id: String, client_secret: String) -> Self {
        Self {
            hub,
            client_id,
            client_secret,
            ctx: RwLock::new(None),
        }
    }

    /// The S2S context, exchanging credentials on first use.
    ///
    /// # Errors
    /// When the authn resolver rejects the credentials.
    pub async fn get(&self) -> Result<SecurityContext, String> {
        if let Some(c) = self.ctx.read().await.clone() {
            return Ok(c);
        }
        let authn = self
            .hub
            .get::<dyn AuthNResolverClient>()
            .map_err(|e| format!("authn resolver unavailable: {e}"))?;
        let res = authn
            .exchange_client_credentials(&ClientCredentialsRequest {
                client_id: self.client_id.clone(),
                client_secret: secrecy::SecretString::from(self.client_secret.clone()),
                scopes: Vec::new(),
            })
            .await
            .map_err(|e| format!("client credentials exchange failed: {e}"))?;
        let c = res.security_context;
        *self.ctx.write().await = Some(c.clone());
        Ok(c)
    }
}

/// Raw response of a proxied call.
pub struct RawResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub gateway_error: bool,
}

impl RawResponse {
    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub struct LlmClient {
    hub: Arc<ClientHub>,
    s2s: Arc<S2sContext>,
    provisioner: Arc<crate::infra::provisioning::Provisioner>,
}

fn failure_from_canonical(e: &CanonicalError) -> LlmFailure {
    if matches!(e, CanonicalError::DeadlineExceeded { .. }) {
        return LlmFailure::timeout();
    }
    if matches!(e, CanonicalError::ResourceExhausted { .. }) {
        return LlmFailure {
            code: "rate_limited",
            message: "Provider is rate limiting requests".to_owned(),
            usage: None,
        };
    }
    tracing::warn!(error = %e, "OAGW proxy request failed");
    LlmFailure::provider("Provider is currently unavailable")
}

/// Map a non-2xx provider/gateway response to a streaming failure.
#[must_use]
pub fn failure_from_status(status: StatusCode, headers: &HeaderMap, body: &[u8], gateway: bool) -> LlmFailure {
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let message = match retry {
            Some(n) => format!("Provider rate limit exceeded; retry in {n}s"),
            None => "Provider rate limit exceeded".to_owned(),
        };
        return LlmFailure {
            code: "rate_limited",
            message,
            usage: None,
        };
    }
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if status == StatusCode::GATEWAY_TIMEOUT
        && (gateway || v.get("type").and_then(Value::as_str).is_some_and(|t| t.contains("deadline_exceeded")))
    {
        return LlmFailure::timeout();
    }
    let msg = error_message(&v)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| format!("Provider returned HTTP {}", status.as_u16()));
    LlmFailure::provider(msg)
}

impl LlmClient {
    #[must_use]
    pub fn new(
        hub: Arc<ClientHub>,
        s2s: Arc<S2sContext>,
        provisioner: Arc<crate::infra::provisioning::Provisioner>,
    ) -> Self {
        Self { hub, s2s, provisioner }
    }

    #[must_use]
    pub fn s2s(&self) -> &Arc<S2sContext> {
        &self.s2s
    }

    #[must_use]
    pub fn provisioner(&self) -> &Arc<crate::infra::provisioning::Provisioner> {
        &self.provisioner
    }

    fn gateway(&self) -> Result<Arc<dyn ServiceGatewayClientV1>, String> {
        self.hub
            .get::<dyn ServiceGatewayClientV1>()
            .map_err(|e| format!("OAGW client unavailable: {e}"))
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Result<http::Response<Body>, CanonicalError> {
        self.provisioner.ensure_ready().await;
        let gw = self.gateway().map_err(|e| CanonicalError::internal(e).create())?;
        let ctx = self
            .s2s
            .get()
            .await
            .map_err(|e| CanonicalError::internal(e).create())?;
        let mut b = http::Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            b = b.header(*k, v);
        }
        let req = b
            .body(body)
            .map_err(|e| CanonicalError::internal(format!("bad request: {e}")).create())?;
        gw.proxy_request(ctx, req).await
    }

    /// Proxied request with a fully buffered response.
    ///
    /// # Errors
    /// Gateway-side failures (`CanonicalError`).
    pub async fn raw(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, String)],
        body: Body,
    ) -> Result<RawResponse, CanonicalError> {
        let resp = self.send(method, uri, headers, body).await?;
        let status = resp.status();
        let gateway_error = resp.extensions().get::<oagw_sdk::api::ErrorSource>()
            == Some(&oagw_sdk::api::ErrorSource::Gateway);
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| CanonicalError::internal(format!("body read: {e}")).create())?;
        Ok(RawResponse {
            status,
            headers,
            body,
            gateway_error,
        })
    }

    fn request_headers(adapter: &dyn Adapter) -> Vec<(&'static str, String)> {
        let mut h = vec![("content-type", "application/json".to_owned())];
        for (k, v) in adapter.extra_headers() {
            h.push((k, v.to_owned()));
        }
        h
    }

    /// Open a streaming provider call and translate its events.
    ///
    /// # Errors
    /// Failure before the stream started (HTTP error, gateway failure).
    pub async fn stream(&self, target: &ChatTarget, req: &LlmRequest) -> Result<EventStream, LlmFailure> {
        let adapter: Arc<dyn Adapter> = Arc::from(adapter_for(target.kind));
        let body = serde_json::to_vec(&adapter.build_body(req)).unwrap_or_default();
        let headers = Self::request_headers(adapter.as_ref());
        let resp = self
            .send(Method::POST, &target.uri(), &headers, Body::from(body))
            .await
            .map_err(|e| failure_from_canonical(&e))?;
        let status = resp.status();
        if !status.is_success() {
            let gateway = resp.extensions().get::<oagw_sdk::api::ErrorSource>()
                == Some(&oagw_sdk::api::ErrorSource::Gateway);
            let headers = resp.headers().clone();
            let bytes = resp.into_body().into_bytes().await.unwrap_or_default();
            return Err(failure_from_status(status, &headers, &bytes, gateway));
        }
        let mut raw = resp.into_body().into_stream();
        let stream = async_stream_events(move |tx| async move {
            let mut parser = SseParser::default();
            let mut state = ParseState::default();
            while let Some(chunk) = raw.next().await {
                match chunk {
                    Ok(bytes) => {
                        for ev in parser.feed(&bytes) {
                            for out in adapter.parse_event(&mut state, ev.event.as_deref(), &ev.data) {
                                let is_term = matches!(out, LlmEvent::Completed(_) | LlmEvent::Failed(_));
                                if tx.send(out).await.is_err() {
                                    return;
                                }
                                if is_term {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "provider stream read failed");
                        let _ = tx.send(LlmEvent::Failed(LlmFailure::provider("Provider stream failed"))).await;
                        return;
                    }
                }
            }
            if let Some(ev) = parser.finish() {
                for out in adapter.parse_event(&mut state, ev.event.as_deref(), &ev.data) {
                    let is_term = matches!(out, LlmEvent::Completed(_) | LlmEvent::Failed(_));
                    let _ = tx.send(out).await;
                    if is_term {
                        return;
                    }
                }
            }
            let _ = tx
                .send(LlmEvent::Failed(LlmFailure::provider("Provider stream ended without a terminal event")))
                .await;
        });
        Ok(stream)
    }

    /// Non-streaming call (thread summary).
    ///
    /// # Errors
    /// Any provider or gateway failure.
    pub async fn complete(&self, target: &ChatTarget, req: &LlmRequest) -> Result<(String, Option<Usage>), LlmFailure> {
        let adapter = adapter_for(target.kind);
        let body = serde_json::to_vec(&adapter.build_body(req)).unwrap_or_default();
        let headers = Self::request_headers(adapter.as_ref());
        let resp = self
            .raw(Method::POST, &target.uri(), &headers, Body::from(body))
            .await
            .map_err(|e| failure_from_canonical(&e))?;
        if !resp.status.is_success() {
            return Err(failure_from_status(resp.status, &resp.headers, &resp.body, resp.gateway_error));
        }
        let is_sse = resp
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
            || resp.body.starts_with(b"event:")
            || resp.body.starts_with(b"data:");
        if is_sse {
            // Tolerate a provider that streams even for `stream: false`.
            let mut parser = SseParser::default();
            let mut state = ParseState::default();
            let mut events = parser.feed(&resp.body);
            events.extend(parser.finish());
            let mut text = String::new();
            for ev in events {
                for out in adapter.parse_event(&mut state, ev.event.as_deref(), &ev.data) {
                    match out {
                        LlmEvent::TextDelta(d) => text.push_str(&d),
                        LlmEvent::Completed(c) => return Ok((text, c.usage)),
                        LlmEvent::Failed(f) => return Err(f),
                        _ => {}
                    }
                }
            }
            return Ok((text, None));
        }
        adapter.parse_complete(&resp.json())
    }
}

/// Build an event stream from a producer task writing into a bounded channel.
fn async_stream_events<F, Fut>(f: F) -> EventStream
where
    F: FnOnce(tokio::sync::mpsc::Sender<LlmEvent>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let handle = tokio::spawn(f(tx));
    Box::pin(AbortOnDrop {
        rx,
        handle: Some(handle),
    })
}

/// Stream that aborts the producer task (closing the provider connection)
/// when dropped.
struct AbortOnDrop {
    rx: tokio::sync::mpsc::Receiver<LlmEvent>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Stream for AbortOnDrop {
    type Item = LlmEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<LlmEvent>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        let mut h = HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, "7".parse().unwrap());
        let f = failure_from_status(StatusCode::TOO_MANY_REQUESTS, &h, b"{}", false);
        assert_eq!(f.code, "rate_limited");
        assert!(f.message.contains("retry in 7s"));
        let f = failure_from_status(StatusCode::GATEWAY_TIMEOUT, &HeaderMap::new(), b"{}", true);
        assert_eq!(f.code, "provider_timeout");
        let f = failure_from_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            &HeaderMap::new(),
            br#"{"error":{"message":"boom resp_abc123"}}"#,
            false,
        );
        assert_eq!(f.code, "provider_error");
        assert_eq!(f.message, "boom [provider_id]");
    }
}
