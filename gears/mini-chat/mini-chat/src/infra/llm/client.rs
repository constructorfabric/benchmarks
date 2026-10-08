//! OAGW transport of the `llm_provider` library: every provider call goes
//! through the in-process OAGW proxy (`ServiceGatewayClientV1`) to
//! `/{alias}{path}`.

use std::pin::Pin;
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use http::{HeaderMap, Method, StatusCode};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::{
    Body, ServerEvent, ServerEventsResponse, ServerEventsStream, ServiceGatewayClientV1,
};
use serde_json::Value;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use super::adapters::{self, StreamParser};
use super::registry::ResolvedProvider;
use super::types::{LlmRequest, ProviderErrorCode, ProviderEvent, ProviderFailure};
use crate::config::ProviderKind;

pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// Shared OAGW client plus the S2S security context used for provider calls.
pub struct LlmClient {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: RwLock<Option<SecurityContext>>,
}

/// Raw HTTP outcome of a non-streaming call.
#[derive(Debug)]
pub struct HttpOutcome {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
    pub from_gateway: bool,
}

impl HttpOutcome {
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    #[must_use]
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

/// Error of a raw call.
#[derive(Debug, Clone)]
pub enum CallError {
    /// The gateway rejected or failed the call.
    Gateway(String, bool),
}

impl CallError {
    #[must_use]
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Gateway(_, true))
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gateway(m, _) => write!(f, "gateway error: {m}"),
        }
    }
}

fn gateway_error(err: &CanonicalError) -> CallError {
    let timeout = matches!(err, CanonicalError::DeadlineExceeded { .. });
    CallError::Gateway(format!("{err}"), timeout)
}

impl LlmClient {
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>) -> Self {
        Self {
            oagw,
            s2s: RwLock::new(None),
        }
    }

    #[must_use]
    pub fn oagw(&self) -> &Arc<dyn ServiceGatewayClientV1> {
        &self.oagw
    }

    pub fn set_s2s(&self, ctx: SecurityContext) {
        if let Ok(mut g) = self.s2s.write() {
            *g = Some(ctx);
        }
    }

    #[must_use]
    pub fn s2s(&self) -> Option<SecurityContext> {
        self.s2s.read().ok().and_then(|g| g.clone())
    }

    fn call_ctx(&self, fallback: Option<&SecurityContext>) -> SecurityContext {
        self.s2s()
            .or_else(|| fallback.cloned())
            .unwrap_or_else(SecurityContext::anonymous)
    }

    /// Send one request through OAGW.
    pub async fn send(
        &self,
        ctx: Option<&SecurityContext>,
        method: Method,
        uri: &str,
        content_type: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: Body,
    ) -> Result<http::Response<Body>, CallError> {
        let mut builder = http::Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            builder = builder.header(http::header::CONTENT_TYPE, ct);
        }
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let req = builder
            .body(body)
            .map_err(|e| CallError::Gateway(format!("invalid request: {e}"), false))?;
        self.oagw
            .proxy_request(self.call_ctx(ctx), req)
            .await
            .map_err(|e| gateway_error(&e))
    }

    /// Send and buffer the response body.
    pub async fn send_buffered(
        &self,
        ctx: Option<&SecurityContext>,
        method: Method,
        uri: &str,
        content_type: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: Body,
    ) -> Result<HttpOutcome, CallError> {
        let resp = self
            .send(ctx, method, uri, content_type, extra_headers, body)
            .await?;
        let status = resp.status();
        let from_gateway = is_gateway_response(&resp);
        let headers = resp.headers().clone();
        let body = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| CallError::Gateway(format!("body read failed: {e}"), false))?;
        Ok(HttpOutcome {
            status,
            headers,
            body,
            from_gateway,
        })
    }

    /// Start a streaming chat request; the stream ends after exactly one
    /// terminal event (`Completed`, `Failed`) or one `FunctionCall`.
    pub async fn stream_chat(
        &self,
        ctx: Option<&SecurityContext>,
        target: &ResolvedProvider,
        req: &LlmRequest,
    ) -> Result<EventStream, ProviderFailure> {
        let body = adapters::build_body(target.kind, req);
        let uri = chat_uri(target, &req.model);
        let payload = serde_json::to_vec(&body)
            .map_err(|e| ProviderFailure::new(ProviderErrorCode::ProviderError, e.to_string()))?;
        let resp = self
            .send(
                ctx,
                Method::POST,
                &uri,
                Some("application/json"),
                &provider_headers(target.kind),
                Body::from(payload),
            )
            .await
            .map_err(|e| call_failure(&e))?;
        let status = resp.status();
        if !status.is_success() {
            let from_gateway = is_gateway_response(&resp);
            let headers = resp.headers().clone();
            let body = resp.into_body().into_bytes().await.unwrap_or_default();
            return Err(http_failure(status, &headers, &body, from_gateway));
        }
        let parser = StreamParser::new(target.kind);
        let events = match ServerEventsStream::from_response::<ServerEvent>(resp) {
            ServerEventsResponse::Events(events) => events,
            ServerEventsResponse::Response(resp) => {
                // A JSON (non-SSE) 2xx body: treat it as a complete response.
                let body = resp.into_body().into_bytes().await.unwrap_or_default();
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let (text, usage) = adapters::parse_nonstream(target.kind, &v);
                let mut out = Vec::new();
                if !text.is_empty() {
                    out.push(ProviderEvent::TextDelta(text));
                }
                out.push(ProviderEvent::Completed {
                    response_id: v.get("id").and_then(Value::as_str).map(str::to_owned),
                    usage,
                    citations: adapters::openai_responses::citations_from_output(&v),
                    incomplete_reason: None,
                });
                return Ok(Box::pin(futures::stream::iter(out)));
            }
        };
        let stream = futures::stream::unfold(
            (events, parser, false),
            |(mut events, mut parser, finished)| async move {
                if finished {
                    return None;
                }
                loop {
                    match events.next().await {
                        Some(Ok(ev)) => {
                            let out = parser.push(ev.event.as_deref(), &ev.data);
                            if out.is_empty() {
                                continue;
                            }
                            let terminal = out.iter().any(is_terminal);
                            return Some((futures::stream::iter(out), (events, parser, terminal)));
                        }
                        Some(Err(e)) => {
                            let f = ProviderFailure::new(
                                ProviderErrorCode::ProviderError,
                                format!("Provider stream failed: {e}"),
                            );
                            return Some((
                                futures::stream::iter(vec![ProviderEvent::Failed(f)]),
                                (events, parser, true),
                            ));
                        }
                        None => {
                            let mut out = parser.eof();
                            if !out.iter().any(is_terminal) {
                                out.push(ProviderEvent::Failed(ProviderFailure::new(
                                    ProviderErrorCode::ProviderError,
                                    "Provider stream ended without a terminal event",
                                )));
                            }
                            return Some((futures::stream::iter(out), (events, parser, true)));
                        }
                    }
                }
            },
        )
        .flatten();
        Ok(Box::pin(stream))
    }

    /// Non-streaming completion (thread summary).
    pub async fn complete(
        &self,
        ctx: Option<&SecurityContext>,
        target: &ResolvedProvider,
        req: &LlmRequest,
    ) -> Result<(String, Option<mini_chat_sdk::UsageTokens>), ProviderFailure> {
        let body = adapters::build_body(target.kind, req);
        let uri = chat_uri(target, &req.model);
        let payload = serde_json::to_vec(&body)
            .map_err(|e| ProviderFailure::new(ProviderErrorCode::ProviderError, e.to_string()))?;
        let out = self
            .send_buffered(
                ctx,
                Method::POST,
                &uri,
                Some("application/json"),
                &provider_headers(target.kind),
                Body::from(payload),
            )
            .await
            .map_err(|e| call_failure(&e))?;
        if !out.is_success() {
            return Err(http_failure(
                out.status,
                &out.headers,
                &out.body,
                out.from_gateway,
            ));
        }
        let v = out.json();
        if v.get("error").is_some_and(Value::is_object) {
            return Err(adapters::openai_responses::failure_from_error_payload(
                &v, "",
            ));
        }
        Ok(adapters::parse_nonstream(target.kind, &v))
    }
}

fn is_terminal(ev: &ProviderEvent) -> bool {
    matches!(
        ev,
        ProviderEvent::Completed { .. }
            | ProviderEvent::Failed(_)
            | ProviderEvent::FunctionCall { .. }
    )
}

/// Extra headers of a provider kind.
#[must_use]
pub fn provider_headers(kind: ProviderKind) -> Vec<(&'static str, &'static str)> {
    match kind {
        ProviderKind::AnthropicMessages => vec![(
            "anthropic-version",
            super::adapters::anthropic::ANTHROPIC_VERSION,
        )],
        _ => vec![("accept", "text/event-stream")],
    }
}

/// `/{alias}{api_path}` with `{model}` substituted.
#[must_use]
pub fn chat_uri(target: &ResolvedProvider, model: &str) -> String {
    format!(
        "/{}{}",
        target.alias,
        target.api_path.replace("{model}", model)
    )
}

fn is_gateway_response(resp: &http::Response<Body>) -> bool {
    resp.extensions().get::<ErrorSource>().copied() == Some(ErrorSource::Gateway)
        || resp
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("gateway"))
}

fn call_failure(e: &CallError) -> ProviderFailure {
    tracing::warn!(error = %e, "provider call failed at the gateway");
    if e.is_timeout() {
        ProviderFailure::new(
            ProviderErrorCode::ProviderTimeout,
            "Provider request timed out",
        )
    } else {
        ProviderFailure::new(
            ProviderErrorCode::ProviderError,
            "Provider is currently unavailable",
        )
    }
}

/// Map a non-2xx provider (or gateway) response to a failure.
#[must_use]
pub fn http_failure(
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
    from_gateway: bool,
) -> ProviderFailure {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry = headers
            .get(http::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok());
        let msg = retry.map_or_else(
            || "Provider rate limit exceeded".to_owned(),
            |n| format!("Provider rate limit exceeded, retry in {n}s"),
        );
        return ProviderFailure::new(ProviderErrorCode::RateLimited, msg);
    }
    let gateway_problem = from_gateway
        || v.get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("gts://"));
    if status == StatusCode::GATEWAY_TIMEOUT && gateway_problem {
        return ProviderFailure::new(
            ProviderErrorCode::ProviderTimeout,
            "Provider request timed out",
        );
    }
    if gateway_problem {
        tracing::warn!(%status, body = %String::from_utf8_lossy(body), "gateway error on provider call");
        return ProviderFailure::new(
            ProviderErrorCode::ProviderError,
            "Provider is currently unavailable",
        );
    }
    let message = v
        .get("error")
        .and_then(|e| e.get("message").or_else(|| e.as_str().map(|_| e)))
        .and_then(Value::as_str)
        .or_else(|| v.get("message").and_then(Value::as_str))
        .map_or_else(
            || format!("Provider returned HTTP {}", status.as_u16()),
            str::to_owned,
        );
    ProviderFailure::new(ProviderErrorCode::ProviderError, message)
}
