//! [`LlmPort`] over the in-process OAGW proxy (`ServiceGatewayClientV1`).
//!
//! Requests go to `/{alias}{api_path}` with the S2S security context (S§1.2);
//! the adapter of the target's `kind` builds the body and parses the stream.
//! Provider SSE chunks are parsed and yielded as they arrive (no buffering).

use std::collections::VecDeque;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::{Body, BodyStream, BoxError};
use oagw_sdk::{ServiceGatewayClientV1, ServiceGatewayError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use tracing::warn;

use crate::domain::ports::LlmPort;
use crate::infra::llm::provider_resolver::ProviderTarget;
use crate::infra::llm::providers::{ParseState, ProviderAdapter, adapter_for};
use crate::infra::llm::sanitize::sanitize_provider_message;
use crate::infra::llm::sse_parser::SseParser;
use crate::infra::llm::types::{
    LlmCompletion, LlmEvent, LlmRequest, ProviderFailure, StreamErrorCode,
};
use crate::infra::s2s::S2sContextProvider;

/// Bytes of an error response body read for the error message.
pub(crate) const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// Bytes of a non-streaming response body.
const COMPLETION_BODY_LIMIT: usize = 8 * 1024 * 1024;

const MSG_UNAVAILABLE: &str = "Provider is currently unavailable";
const MSG_TIMEOUT: &str = "Provider request timed out";
const MSG_RATE_LIMITED: &str = "Provider rate limit exceeded";
const MSG_STREAM_FAILED: &str = "Provider stream failed";
const MSG_STREAM_ENDED: &str = "Provider stream ended unexpectedly";
const MSG_INVALID_RESPONSE: &str = "Provider returned an invalid response";

/// OAGW-backed LLM gateway.
pub struct LlmGateway {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
}

impl LlmGateway {
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContextProvider>) -> Self {
        Self { oagw, s2s }
    }

    /// Send `req` and return the successful (2xx) response.
    async fn send(
        &self,
        target: &ProviderTarget,
        adapter: &dyn ProviderAdapter,
        req: &LlmRequest,
    ) -> Result<http::Response<Body>, ProviderFailure> {
        let ctx = self.s2s.get().await.map_err(|e| {
            warn!(error = %e, "S2S security context unavailable for the provider call");
            failure(StreamErrorCode::ProviderError, MSG_UNAVAILABLE)
        })?;
        let body = serde_json::to_vec(&adapter.build_body(req)).map_err(|e| {
            warn!(error = %e, "provider request body serialization failed");
            failure(StreamErrorCode::ProviderError, MSG_UNAVAILABLE)
        })?;
        let accept = if req.stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        let mut builder = http::Request::builder()
            .method(http::Method::POST)
            .uri(target.chat_uri(&req.model))
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, accept);
        for (name, value) in adapter.extra_headers(req) {
            builder = builder.header(name, value);
        }
        let http_req = builder.body(Body::from(body)).map_err(|e| {
            warn!(error = %e, provider = %target.provider_id, "invalid provider request URI");
            failure(StreamErrorCode::ProviderError, MSG_UNAVAILABLE)
        })?;
        let resp = self
            .oagw
            .proxy_request(ctx, http_req)
            .await
            .map_err(|e| gateway_error(target, e))?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(http_error(target, resp).await)
        }
    }
}

fn failure(code: StreamErrorCode, message: &str) -> ProviderFailure {
    ProviderFailure::new(code, message)
}

fn rate_limited(retry_after_secs: Option<u64>) -> ProviderFailure {
    let message = match retry_after_secs {
        Some(secs) => format!("{MSG_RATE_LIMITED}, retry in {secs}s"),
        None => MSG_RATE_LIMITED.to_owned(),
    };
    ProviderFailure::new(StreamErrorCode::RateLimited, message)
}

/// Error returned by the proxy itself (the request did not get a response).
fn gateway_error(target: &ProviderTarget, err: CanonicalError) -> ProviderFailure {
    warn!(provider = %target.provider_id, error = %err.detail(), "OAGW proxy call failed");
    match ServiceGatewayError::from(err) {
        ServiceGatewayError::Timeout => failure(StreamErrorCode::ProviderTimeout, MSG_TIMEOUT),
        ServiceGatewayError::RateLimited { retry_after_secs } => rate_limited(retry_after_secs),
        _ => failure(StreamErrorCode::ProviderError, MSG_UNAVAILABLE),
    }
}

/// Non-2xx response: 429 → `rate_limited`, the gateway's own 504 →
/// `provider_timeout`, anything else → `provider_error` with the sanitized
/// provider message.
async fn http_error(target: &ProviderTarget, resp: http::Response<Body>) -> ProviderFailure {
    let status = resp.status();
    let from_gateway = resp.extensions().get::<ErrorSource>() == Some(&ErrorSource::Gateway);
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = read_limited(resp.into_body(), ERROR_BODY_LIMIT)
        .await
        .unwrap_or_default();
    let json: Option<Value> = serde_json::from_slice(&body).ok();
    warn!(
        provider = %target.provider_id,
        status = status.as_u16(),
        from_gateway,
        body = %String::from_utf8_lossy(&body[..body.len().min(2048)]),
        "provider request failed"
    );

    if status == http::StatusCode::TOO_MANY_REQUESTS {
        return rate_limited(retry_after);
    }
    let problem_deadline = json
        .as_ref()
        .and_then(|j| j.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t.contains("deadline_exceeded"));
    if status == http::StatusCode::GATEWAY_TIMEOUT && (from_gateway || problem_deadline) {
        return failure(StreamErrorCode::ProviderTimeout, MSG_TIMEOUT);
    }
    let provider_message = if from_gateway {
        None
    } else {
        json.as_ref().and_then(error_message)
    };
    match provider_message {
        Some(msg) => ProviderFailure::new(
            StreamErrorCode::ProviderError,
            sanitize_provider_message(&msg),
        ),
        None => failure(StreamErrorCode::ProviderError, MSG_UNAVAILABLE),
    }
}

/// Provider error message of a JSON error body (`error.message`, a string
/// `error`, or a top-level `message`).
pub(crate) fn error_message(body: &Value) -> Option<String> {
    let msg = body
        .pointer("/error/message")
        .or_else(|| body.get("error").filter(|e| e.is_string()))
        .or_else(|| body.get("message"))?
        .as_str()?
        .trim();
    (!msg.is_empty()).then(|| msg.to_owned())
}

/// Read at most `limit` bytes of `body` (the rest is dropped).
pub(crate) async fn read_limited(body: Body, limit: usize) -> Result<Bytes, ()> {
    let mut stream = body.into_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ())?;
        let room = limit.saturating_sub(buf.len());
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= limit {
            break;
        }
    }
    Ok(Bytes::from(buf))
}

/// State of one provider event stream.
struct StreamState {
    body: BodyStream,
    adapter: &'static dyn ProviderAdapter,
    parser: SseParser,
    parse: ParseState,
    pending: VecDeque<LlmEvent>,
    done: bool,
    cancel: CancellationToken,
}

impl StreamState {
    /// Next event: pending ones first, then read the body until a chunk
    /// completes at least one event. Ends after the terminal event, at EOF
    /// (a synthesized failure when no terminal event came) or on cancel.
    async fn next(mut self) -> Option<(LlmEvent, Self)> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                if ev.is_terminal() {
                    self.finish();
                }
                return Some((ev, self));
            }
            if self.done {
                return None;
            }
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return None,
                chunk = self.body.next() => self.on_chunk(chunk),
            }
        }
    }

    /// Parse one body chunk (`None` = EOF) into pending events.
    fn on_chunk(&mut self, chunk: Option<Result<Bytes, BoxError>>) {
        match chunk {
            Some(Ok(bytes)) => {
                for e in self.parser.push(&bytes) {
                    self.pending
                        .extend(self.adapter.parse_event(&e, &mut self.parse));
                }
            }
            Some(Err(e)) => {
                warn!(error = %e, "provider stream read failed");
                self.pending.push_back(LlmEvent::Failed(failure(
                    StreamErrorCode::ProviderError,
                    MSG_STREAM_FAILED,
                )));
            }
            None => {
                if let Some(e) = self.parser.finish() {
                    self.pending
                        .extend(self.adapter.parse_event(&e, &mut self.parse));
                }
                if !self.pending.iter().any(LlmEvent::is_terminal) {
                    warn!("provider stream ended without a terminal event");
                    self.pending.push_back(LlmEvent::Failed(failure(
                        StreamErrorCode::ProviderError,
                        MSG_STREAM_ENDED,
                    )));
                }
            }
        }
    }

    /// Terminal event emitted: release the provider connection now.
    fn finish(&mut self) {
        self.done = true;
        self.pending.clear();
        self.body = Box::pin(futures::stream::empty());
    }
}

#[async_trait]
impl LlmPort for LlmGateway {
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, ProviderFailure> {
        let adapter = adapter_for(target.kind);
        let resp = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(futures::stream::empty().boxed()),
            resp = self.send(target, adapter, &req) => resp?,
        };
        let state = StreamState {
            body: resp.into_body().into_stream(),
            adapter,
            parser: SseParser::default(),
            parse: ParseState::default(),
            pending: VecDeque::new(),
            done: false,
            cancel,
        };
        Ok(futures::stream::unfold(state, StreamState::next).boxed())
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, ProviderFailure> {
        let adapter = adapter_for(target.kind);
        let resp = self.send(target, adapter, &req).await?;
        let body = read_limited(resp.into_body(), COMPLETION_BODY_LIMIT)
            .await
            .map_err(|()| failure(StreamErrorCode::ProviderError, MSG_STREAM_FAILED))?;
        let json: Value = serde_json::from_slice(&body).map_err(|e| {
            warn!(provider = %target.provider_id, error = %e, "provider returned non-JSON body");
            failure(StreamErrorCode::ProviderError, MSG_INVALID_RESPONSE)
        })?;
        adapter.parse_completion(&json)
    }
}

#[cfg(test)]
#[path = "gateway_tests.rs"]
mod tests;
