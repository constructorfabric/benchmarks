//! [`LlmClient`]: sends adapter requests through the OAGW in-process proxy
//! (`ServiceGatewayClientV1::proxy_request`) with the gear's S2S security
//! context and turns the response into [`LlmEvent`]s.
//!
//! Error mapping (spec §8.2/§11.3): a pre-stream `proxy_request` error maps
//! through [`ServiceGatewayError`]; a non-2xx response is told apart by the
//! OAGW [`ErrorSource`] extension — the gateway's own timeout (504
//! `deadline_exceeded` Problem) is `Timeout`, a provider 429 is `RateLimited`
//! (numeric `Retry-After`), any other provider status is `Provider` with the
//! provider's `error.message`.

use std::collections::VecDeque;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use http::{Method, StatusCode, header};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::body::{BodyStream, BoxError};
use oagw_sdk::{Body, ServiceGatewayClientV1, ServiceGatewayError};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::sse_reader::SseReader;
use super::{
    CompletionResult, LlmError, LlmEvent, LlmRequest, ParseState, ProviderAdapter,
    ResolvedProvider, adapter_for,
};
use crate::infra::oagw::provisioning::Provisioner;
use crate::infra::oagw::s2s::S2sContext;

/// Client-visible text of failures whose details are internal (logged only).
const UNAVAILABLE: &str = "Provider is currently unavailable";

/// `Unavailable` with the generic text; `detail` is only logged.
fn unavailable(detail: &dyn std::fmt::Display) -> LlmError {
    warn!(%detail, "provider call failed before reaching the provider");
    LlmError::Unavailable(UNAVAILABLE.to_owned())
}

/// Provider client over OAGW.
pub struct LlmClient {
    gw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
    /// Provisions a deferred provider on demand before a request.
    provisioner: Option<Arc<Provisioner>>,
}

impl LlmClient {
    #[must_use]
    pub fn new(gw: Arc<dyn ServiceGatewayClientV1>, s2s: Arc<S2sContext>) -> Self {
        Self {
            gw,
            s2s,
            provisioner: None,
        }
    }

    /// Provision a still-deferred provider (rate-limited) before its requests.
    #[must_use]
    pub fn with_provisioner(mut self, provisioner: Arc<Provisioner>) -> Self {
        self.provisioner = Some(provisioner);
        self
    }

    /// Start a streaming call. Events are forwarded as soon as they are parsed;
    /// the stream always ends with a terminal event (`Completed` / `Failed`; a
    /// body error or an end without terminal becomes `Failed`), except after
    /// `cancel`, which ends it at once and drops the provider response body.
    ///
    /// # Errors
    /// Failures before the stream opens (gateway error, provider non-2xx).
    pub async fn stream(
        &self,
        p: &ResolvedProvider,
        req: &LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, LlmError> {
        let adapter = adapter_for(p.kind);
        let resp = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return Err(LlmError::Unavailable("request cancelled".to_owned()));
            }
            r = self.send(p, req, adapter.as_ref()) => r?,
        };
        Ok(event_stream(
            adapter,
            resp.into_body().into_stream(),
            cancel,
        ))
    }

    /// Non-streaming call.
    ///
    /// # Errors
    /// Gateway or provider failure, or an invalid response body.
    pub async fn complete(
        &self,
        p: &ResolvedProvider,
        req: &LlmRequest,
    ) -> Result<CompletionResult, LlmError> {
        let adapter = adapter_for(p.kind);
        let resp = self.send(p, req, adapter.as_ref()).await?;
        let body = resp
            .into_body()
            .into_bytes()
            .await
            .map_err(|e| body_error(e.as_ref()))?;
        adapter.parse_completion(&body)
    }

    /// Proxy the request; `Ok` only for a 2xx response.
    async fn send(
        &self,
        p: &ResolvedProvider,
        req: &LlmRequest,
        adapter: &dyn ProviderAdapter,
    ) -> Result<http::Response<Body>, LlmError> {
        let refreshed = match &self.provisioner {
            Some(provisioner) => provisioner.ensure_provisioned(p).await,
            None => None,
        };
        let p = refreshed.as_ref().unwrap_or(p);
        let ctx = self.s2s.get().map_err(|e| unavailable(&e))?;
        let body = serde_json::to_vec(&adapter.build_body(req))
            .map_err(|e| unavailable(&format!("cannot encode provider request: {e}")))?;
        let mut builder = http::Request::builder()
            .method(Method::POST)
            .uri(p.chat_uri(&req.model))
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in adapter.headers(req) {
            builder = builder.header(name, value);
        }
        let http_req = builder
            .body(Body::from(body))
            .map_err(|e| unavailable(&format!("cannot build provider request: {e}")))?;
        let resp = self
            .gw
            .proxy_request(ctx, http_req)
            .await
            .map_err(|e| gateway_error(&ServiceGatewayError::from(e)))?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(http_error(resp).await)
        }
    }
}

/// Maps a pre-stream `proxy_request` failure.
pub(crate) fn gateway_error(err: &ServiceGatewayError) -> LlmError {
    match err {
        ServiceGatewayError::RateLimited { retry_after_secs } => LlmError::RateLimited {
            retry_after_secs: *retry_after_secs,
            message: "rate limit exceeded".to_owned(),
        },
        ServiceGatewayError::Timeout => LlmError::Timeout("provider request timed out".to_owned()),
        other => unavailable(&format!("gateway error: {other}")),
    }
}

/// Numeric `Retry-After` seconds.
fn retry_after(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Provider error text of a JSON error body (`error.message`, `error`, `message`).
fn provider_message(body: Option<&Value>) -> Option<String> {
    let body = body?;
    let text = match body.get("error") {
        Some(Value::Object(e)) => e.get("message").and_then(Value::as_str),
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
    .or_else(|| body.get("message").and_then(Value::as_str))?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Maps a non-2xx proxy response.
async fn http_error(resp: http::Response<Body>) -> LlmError {
    let status = resp.status();
    let source = resp
        .extensions()
        .get::<ErrorSource>()
        .copied()
        .unwrap_or(ErrorSource::Upstream);
    let retry = retry_after(resp.headers());
    let body = resp.into_body().into_bytes().await.unwrap_or_default();
    let json = serde_json::from_slice::<Value>(&body).ok();

    match source {
        ErrorSource::Gateway => {
            let problem_type = json
                .as_ref()
                .and_then(|v| v.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if status == StatusCode::GATEWAY_TIMEOUT || problem_type.contains("deadline_exceeded") {
                LlmError::Timeout("gateway timeout".to_owned())
            } else if status == StatusCode::TOO_MANY_REQUESTS {
                LlmError::RateLimited {
                    retry_after_secs: retry,
                    message: "rate limit exceeded".to_owned(),
                }
            } else {
                unavailable(&format!(
                    "gateway error (HTTP {}): {}",
                    status.as_u16(),
                    String::from_utf8_lossy(&body)
                ))
            }
        }
        ErrorSource::Upstream => {
            let message = provider_message(json.as_ref());
            if status == StatusCode::TOO_MANY_REQUESTS {
                LlmError::RateLimited {
                    retry_after_secs: retry,
                    message: message.unwrap_or_else(|| "provider rate limit exceeded".to_owned()),
                }
            } else {
                LlmError::Provider {
                    message: message
                        .unwrap_or_else(|| format!("provider returned HTTP {}", status.as_u16())),
                }
            }
        }
    }
}

/// Maps a failure while reading the provider body.
fn body_error(e: &(dyn std::error::Error + Send + Sync)) -> LlmError {
    let text = e.to_string().to_ascii_lowercase();
    if text.contains("timed out") || text.contains("timeout") {
        LlmError::Timeout("provider stream timed out".to_owned())
    } else {
        LlmError::Unavailable("provider stream failed".to_owned())
    }
}

struct StreamState {
    adapter: Arc<dyn ProviderAdapter>,
    body: BodyStream,
    reader: SseReader,
    parse: ParseState,
    pending: VecDeque<LlmEvent>,
    cancel: CancellationToken,
    done: bool,
}

impl StreamState {
    fn parse_chunk(&mut self, chunk: &[u8]) {
        for frame in self.reader.push(chunk) {
            let events = self
                .adapter
                .parse_event(&mut self.parse, &frame.event, &frame.data);
            self.pending.extend(events);
        }
    }

    fn end_of_body(&mut self) {
        if let Some(frame) = self.reader.finish() {
            let events = self
                .adapter
                .parse_event(&mut self.parse, &frame.event, &frame.data);
            self.pending.extend(events);
        }
        if !self.pending.iter().any(LlmEvent::is_terminal) {
            self.pending.push_back(LlmEvent::Failed {
                error: LlmError::Provider {
                    message: "provider stream ended without a terminal event".to_owned(),
                },
                usage: None,
            });
        }
    }
}

fn event_stream(
    adapter: Arc<dyn ProviderAdapter>,
    body: BodyStream,
    cancel: CancellationToken,
) -> BoxStream<'static, LlmEvent> {
    let state = StreamState {
        adapter,
        body,
        reader: SseReader::new(),
        parse: ParseState::default(),
        pending: VecDeque::new(),
        cancel,
        done: false,
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if s.cancel.is_cancelled() {
                return None;
            }
            if let Some(ev) = s.pending.pop_front() {
                if ev.is_terminal() {
                    // Nothing after the terminal event: release the provider body now.
                    s.done = true;
                    s.pending.clear();
                    s.body = Box::pin(futures::stream::empty());
                }
                return Some((ev, s));
            }
            if s.done {
                return None;
            }
            let next: Option<Result<bytes::Bytes, BoxError>> = tokio::select! {
                biased;
                () = s.cancel.cancelled() => return None,
                chunk = s.body.next() => chunk,
            };
            match next {
                Some(Ok(chunk)) => s.parse_chunk(&chunk),
                Some(Err(e)) => s.pending.push_back(LlmEvent::Failed {
                    error: body_error(e.as_ref()),
                    usage: None,
                }),
                None => s.end_of_body(),
            }
        }
    })
    .boxed()
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
