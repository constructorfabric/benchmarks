//! OAGW-backed implementation of [`LlmClient`] and [`FileStorage`].
//!
//! OWNER: LLM adapter work package. Builds provider requests per adapter kind, sends them
//! through `ServiceGatewayClientV1::proxy_request` to `/{alias}{path}` using the gear S2S
//! context, parses SSE into [`LlmEvent`]s and maps errors.

mod errors;
mod knowledge;
mod request;
mod storage;
mod translate;

#[cfg(test)]
pub(crate) mod fake_gateway;

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use http::{HeaderValue, Method};
use oagw_sdk::api::ErrorSource;
use oagw_sdk::sse::{ServerEvent, ServerEventsResponse, ServerEventsStream};
use oagw_sdk::{Body, ServiceGatewayClientV1};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    LlmClient, LlmCompletion, LlmEvent, LlmFailure, LlmRequest, LlmTextResult, ProviderResolver,
    ResolvedProvider,
};
use crate::config::ProviderKind;
use crate::domain::error::stream_codes::PROVIDER_ERROR;
use crate::infra::s2s::S2sContextProvider;
use errors::{
    INVALID_RESPONSE_MESSAGE, UNAVAILABLE_MESSAGE, failure, gateway_failure, http_failure,
    read_body, stream_failure,
};
use translate::{
    AnthropicTranslator, ChatCompletionsTranslator, ResponsesTranslator, Translator, parse_usage,
    responses_citations, responses_function_call, responses_output_text, strip_think,
};

/// Provider client over the OAGW in-process proxy.
pub struct OagwProviderClient {
    resolver: Arc<ProviderResolver>,
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
}

/// Outcome of a gateway call that already reached the provider.
enum Sent {
    Response(http::Response<Body>),
    Cancelled,
}

impl OagwProviderClient {
    #[must_use]
    pub fn new(
        resolver: Arc<ProviderResolver>,
        oagw: Arc<dyn ServiceGatewayClientV1>,
        s2s: Arc<S2sContextProvider>,
    ) -> Self {
        Self {
            resolver,
            oagw,
            s2s,
        }
    }

    fn resolve_chat(&self, req: &LlmRequest) -> Result<ResolvedProvider, LlmFailure> {
        self.resolver
            .resolve(&req.provider_id, req.tenant_id)
            .ok_or_else(|| {
                tracing::error!(provider_id = %req.provider_id, "unknown provider id");
                failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE)
            })
    }

    /// Builds the HTTP request of a chat / summary call.
    fn chat_request(
        rp: &ResolvedProvider,
        req: &LlmRequest,
    ) -> Result<http::Request<Body>, LlmFailure> {
        let body = request::build_body(rp.kind, req);
        let uri = format!(
            "/{}{}",
            rp.alias,
            request::chat_path(&rp.api_path, &req.model)
        );
        let mut builder = http::Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(http::header::CONTENT_TYPE, "application/json");
        if req.stream {
            builder = builder.header(http::header::ACCEPT, "text/event-stream");
        }
        if rp.kind == ProviderKind::AnthropicMessages {
            builder = builder.header("anthropic-version", request::ANTHROPIC_VERSION);
        }
        let bytes = serde_json::to_vec(&body).map_err(|e| {
            tracing::error!(error = %e, "failed to serialize provider request");
            failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE)
        })?;
        builder.body(Body::from(bytes)).map_err(|e| {
            tracing::error!(error = %e, "failed to build provider request");
            failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE)
        })
    }

    /// Sends a request via OAGW with the S2S context. Non-2xx responses are returned as
    /// failures (body read and mapped).
    async fn send(
        &self,
        http_req: http::Request<Body>,
        cancel: Option<&CancellationToken>,
    ) -> Result<Sent, LlmFailure> {
        let ctx = self.s2s.get().await.map_err(|e| {
            tracing::warn!(error = %e, "S2S security context unavailable for provider call");
            failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE)
        })?;
        let call = self.oagw.proxy_request(ctx, http_req);
        let result = match cancel {
            Some(cancel) => tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(Sent::Cancelled),
                r = call => r,
            },
            None => call.await,
        };
        let resp = result.map_err(|e| gateway_failure(&e))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(Sent::Response(resp));
        }
        let source = resp.extensions().get::<ErrorSource>().copied();
        let (parts, body) = resp.into_parts();
        let bytes = read_body(body).await;
        Err(http_failure(status, &parts.headers, source, &bytes))
    }
}

fn is_json(resp: &http::Response<Body>) -> bool {
    resp.headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("application/json"))
}

fn translator_for(kind: ProviderKind) -> Box<dyn Translator> {
    match kind {
        ProviderKind::OpenaiResponses => Box::new(ResponsesTranslator::new(false)),
        ProviderKind::VllmResponses => Box::new(ResponsesTranslator::new(true)),
        ProviderKind::OpenaiChatCompletions => Box::new(ChatCompletionsTranslator::default()),
        ProviderKind::AnthropicMessages => Box::new(AnthropicTranslator::default()),
    }
}

fn is_terminal(e: &LlmEvent) -> bool {
    matches!(e, LlmEvent::Completed(_) | LlmEvent::Failed(_))
}

/// Translates an SSE stream until the first terminal event. Cancellation drops the upstream
/// body (closing the connection) and ends the stream without a terminal event.
fn drive(
    mut events: ServerEventsStream<ServerEvent>,
    mut translator: Box<dyn Translator>,
    cancel: CancellationToken,
) -> BoxStream<'static, LlmEvent> {
    Box::pin(async_stream::stream! {
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                n = events.next() => Some(n),
            };
            let Some(next) = next else {
                tracing::debug!("provider stream cancelled; dropping upstream connection");
                break;
            };
            match next {
                None => {
                    tracing::warn!("provider stream ended without a terminal event");
                    for out in translator.on_end() {
                        yield out;
                    }
                    break;
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "provider stream failed");
                    yield LlmEvent::Failed(failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE));
                    break;
                }
                Some(Ok(ev)) => {
                    let mut done = false;
                    for out in translator.on_event(ev.event.as_deref(), &ev.data) {
                        done = is_terminal(&out);
                        yield out;
                        if done {
                            break;
                        }
                    }
                    if done {
                        break;
                    }
                }
            }
        }
    })
}

/// A 2xx JSON body where a stream was expected: provider error object or a complete
/// (non-streamed) Responses API object.
fn events_from_json_body(kind: ProviderKind, bytes: &[u8]) -> Vec<LlmEvent> {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return vec![LlmEvent::Failed(failure(
            PROVIDER_ERROR,
            INVALID_RESPONSE_MESSAGE,
        ))];
    };
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return vec![LlmEvent::Failed(stream_failure(Some(&v), None, None, None))];
    }
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses
            if v.get("output").is_some() =>
        {
            match parse_complete(kind, &v) {
                Ok(c) => {
                    let mut out = Vec::new();
                    if !c.output_text.is_empty() {
                        out.push(LlmEvent::TextDelta(c.output_text.clone()));
                    }
                    out.extend(
                        v.get("output")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(responses_function_call),
                    );
                    out.push(LlmEvent::Completed(c));
                    out
                }
                Err(f) => vec![LlmEvent::Failed(f)],
            }
        }
        _ => vec![LlmEvent::Failed(failure(
            PROVIDER_ERROR,
            INVALID_RESPONSE_MESSAGE,
        ))],
    }
}

/// Parses a non-streaming response body of `kind`.
fn parse_complete(kind: ProviderKind, v: &Value) -> Result<LlmCompletion, LlmFailure> {
    let id = v.get("id").and_then(Value::as_str).map(str::to_owned);
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            let status = v.get("status").and_then(Value::as_str);
            if status == Some("failed") || v.get("error").is_some_and(|e| !e.is_null()) {
                let wrapped = serde_json::json!({ "response": v });
                return Err(stream_failure(
                    Some(&wrapped),
                    None,
                    parse_usage(v.get("usage")),
                    id,
                ));
            }
            let mut text = responses_output_text(v);
            if kind == ProviderKind::VllmResponses {
                text = strip_think(&text);
            }
            Ok(LlmCompletion {
                response_id: id,
                usage: parse_usage(v.get("usage")),
                citations: responses_citations(v),
                output_text: text,
                incomplete_reason: (status == Some("incomplete")).then(|| {
                    v.pointer("/incomplete_details/reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned()
                }),
            })
        }
        ProviderKind::OpenaiChatCompletions => {
            let text = v
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            Ok(LlmCompletion {
                response_id: id,
                usage: parse_usage(v.get("usage")),
                output_text: text,
                ..LlmCompletion::default()
            })
        }
        ProviderKind::AnthropicMessages => {
            let text: String = v
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            Ok(LlmCompletion {
                response_id: id,
                usage: parse_usage(v.get("usage")),
                output_text: text,
                ..LlmCompletion::default()
            })
        }
    }
}

#[async_trait]
impl LlmClient for OagwProviderClient {
    async fn stream(
        &self,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, LlmEvent>, LlmFailure> {
        let rp = self.resolve_chat(&req)?;
        let http_req = Self::chat_request(&rp, &req)?;
        let resp = match self.send(http_req, Some(&cancel)).await? {
            Sent::Response(r) => r,
            Sent::Cancelled => return Ok(Box::pin(futures::stream::empty())),
        };

        if is_json(&resp) {
            let bytes = read_body(resp.into_body()).await;
            let events = events_from_json_body(rp.kind, &bytes);
            return Ok(Box::pin(futures::stream::iter(events)));
        }

        // Parse as SSE even when the upstream omitted the content type.
        let mut resp = resp;
        resp.headers_mut().insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );
        match ServerEventsStream::from_response::<ServerEvent>(resp) {
            ServerEventsResponse::Events(events) => {
                Ok(drive(events, translator_for(rp.kind), cancel))
            }
            ServerEventsResponse::Response(_) => {
                Err(failure(PROVIDER_ERROR, INVALID_RESPONSE_MESSAGE))
            }
        }
    }

    async fn complete(&self, req: LlmRequest) -> Result<LlmTextResult, LlmFailure> {
        let mut req = req;
        req.stream = false;
        let rp = self.resolve_chat(&req)?;
        let http_req = Self::chat_request(&rp, &req)?;
        let resp = match self.send(http_req, None).await? {
            Sent::Response(r) => r,
            Sent::Cancelled => return Err(failure(PROVIDER_ERROR, UNAVAILABLE_MESSAGE)),
        };
        let status = resp.status();
        let bytes = read_body(resp.into_body()).await;
        let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
            tracing::warn!(error = %e, status = status.as_u16(), "unparseable provider response");
            failure(PROVIDER_ERROR, INVALID_RESPONSE_MESSAGE)
        })?;
        let c = parse_complete(rp.kind, &v)?;
        Ok(LlmTextResult {
            text: c.output_text,
            usage: c.usage,
            response_id: c.response_id,
        })
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod client_tests;
