//! Adapter dispatch: send a chat request and decode the provider stream.

use std::pin::Pin;
use std::sync::Arc;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use oagw_sdk::Body;
use serde_json::Value;

use super::anthropic_messages::AnthropicDecoder;
use super::chat_completions::ChatCompletionsDecoder;
use super::openai_responses::ResponsesDecoder;
use super::sse::{SseFrame, SseParser};
use super::vllm_responses::VllmDecoder;
use super::{
    ChatRequest, ProviderErrorKind, ProviderEvent, ProviderTransport, ProviderUsage, ResolvedProvider, TransportError,
    anthropic_messages, chat_completions, openai_responses, vllm_responses,
};
use crate::config::ProviderKind;

pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

enum Decoder {
    Responses(ResponsesDecoder),
    Chat(ChatCompletionsDecoder),
    Vllm(VllmDecoder),
    Anthropic(AnthropicDecoder),
}

impl Decoder {
    fn new(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenaiResponses => Self::Responses(ResponsesDecoder::default()),
            ProviderKind::OpenaiChatCompletions => Self::Chat(ChatCompletionsDecoder::default()),
            ProviderKind::VllmResponses => Self::Vllm(VllmDecoder::default()),
            ProviderKind::AnthropicMessages => Self::Anthropic(AnthropicDecoder::default()),
        }
    }

    fn on_frame(&mut self, f: &SseFrame) -> Vec<ProviderEvent> {
        match self {
            Self::Responses(d) => d.on_frame(f),
            Self::Chat(d) => d.on_frame(f),
            Self::Vllm(d) => d.on_frame(f),
            Self::Anthropic(d) => d.on_frame(f),
        }
    }

    fn is_terminal(&self) -> bool {
        match self {
            Self::Responses(d) => d.is_terminal(),
            Self::Chat(d) => d.is_terminal(),
            Self::Vllm(d) => d.is_terminal(),
            Self::Anthropic(d) => d.is_terminal(),
        }
    }

    fn on_end(&mut self) -> Vec<ProviderEvent> {
        match self {
            Self::Chat(d) => d.finish(),
            _ => Vec::new(),
        }
    }
}

/// Builds the request body for the provider kind.
#[must_use]
pub fn build_body(kind: ProviderKind, req: &ChatRequest) -> Value {
    match kind {
        ProviderKind::OpenaiResponses => openai_responses::build_body(req, true, true, true),
        ProviderKind::OpenaiChatCompletions => chat_completions::build_body(req),
        ProviderKind::VllmResponses => vllm_responses::build_body(req),
        ProviderKind::AnthropicMessages => anthropic_messages::build_body(req),
    }
}

fn failed(kind: ProviderErrorKind, message: impl Into<String>) -> ProviderEvent {
    ProviderEvent::Failed {
        kind,
        message: message.into(),
        usage: None,
    }
}

fn transport_failure(e: &TransportError) -> ProviderEvent {
    match e {
        TransportError::Timeout(_) => failed(ProviderErrorKind::Timeout, "Provider request timed out"),
        TransportError::Gateway(d) => {
            tracing::warn!(error = %d, "provider gateway error");
            failed(ProviderErrorKind::ProviderError, "Provider is currently unavailable")
        }
    }
}

/// Maps a non-2xx provider response to a terminal failure.
pub async fn error_from_response(resp: http::Response<Body>) -> ProviderEvent {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = resp.into_body().into_bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&body).to_string();
    let parsed: Option<Value> = serde_json::from_str(&text).ok();
    let message = parsed
        .as_ref()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").and_then(Value::as_str).or_else(|| e.as_str()))
                .or_else(|| v.get("message").and_then(Value::as_str))
                .or_else(|| v.get("detail").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .unwrap_or_else(|| {
            if text.trim().is_empty() {
                format!("Provider returned HTTP {}", status.as_u16())
            } else {
                text.chars().take(500).collect()
            }
        });
    if status == http::StatusCode::TOO_MANY_REQUESTS {
        let msg = match retry_after {
            Some(s) => format!("Provider rate limit reached, retry in {s}s"),
            None => "Provider rate limit reached".to_owned(),
        };
        return failed(ProviderErrorKind::RateLimited, msg);
    }
    failed(ProviderErrorKind::ProviderError, message)
}

fn decode_stream(body: Body, kind: ProviderKind) -> EventStream {
    let stream = body.into_stream();
    let state = (stream, SseParser::new(), Decoder::new(kind), Vec::<ProviderEvent>::new(), false);
    Box::pin(futures::stream::unfold(state, |(mut s, mut parser, mut dec, mut queue, mut done)| async move {
        loop {
            if !queue.is_empty() {
                let ev = queue.remove(0);
                return Some((ev, (s, parser, dec, queue, done)));
            }
            if done {
                return None;
            }
            match s.next().await {
                Some(Ok(chunk)) => {
                    let chunk: Bytes = chunk;
                    for f in parser.feed(&chunk) {
                        queue.extend(dec.on_frame(&f));
                    }
                    if dec.is_terminal() {
                        done = true;
                    }
                }
                Some(Err(e)) => {
                    done = true;
                    let msg = e.to_string();
                    let kind = if msg.to_lowercase().contains("timeout") || msg.to_lowercase().contains("timed out") {
                        ProviderErrorKind::Timeout
                    } else {
                        ProviderErrorKind::ProviderError
                    };
                    queue.push(failed(kind, "Provider stream failed"));
                }
                None => {
                    done = true;
                    if let Some(f) = parser.finish() {
                        queue.extend(dec.on_frame(&f));
                    }
                    queue.extend(dec.on_end());
                    if !dec.is_terminal() {
                        queue.push(failed(ProviderErrorKind::ProviderError, "Provider stream ended unexpectedly"));
                    }
                }
            }
        }
    }))
}

/// Sends a streaming chat request and returns the decoded event stream.
/// Failures before streaming are returned as a one-element stream.
pub async fn stream_chat(transport: &Arc<dyn ProviderTransport>, provider: &ResolvedProvider, req: &ChatRequest) -> EventStream {
    let body = build_body(provider.kind, req);
    let uri = provider.chat_uri(&req.model);
    let request = match http::Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "text/event-stream")
        .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
    {
        Ok(r) => r,
        Err(e) => return Box::pin(futures::stream::iter(vec![failed(ProviderErrorKind::ProviderError, e.to_string())])),
    };
    match transport.send(request).await {
        Err(e) => Box::pin(futures::stream::iter(vec![transport_failure(&e)])),
        Ok(resp) if !resp.status().is_success() => {
            let ev = error_from_response(resp).await;
            Box::pin(futures::stream::iter(vec![ev]))
        }
        Ok(resp) => decode_stream(resp.into_body(), provider.kind),
    }
}

/// Result of a non-streaming completion (thread summary).
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: Option<ProviderUsage>,
}

/// Sends a non-streaming request and extracts the output text.
///
/// # Errors
/// The terminal failure event (kind and message).
pub async fn complete(
    transport: &Arc<dyn ProviderTransport>,
    provider: &ResolvedProvider,
    req: &ChatRequest,
) -> Result<Completion, (ProviderErrorKind, String)> {
    let body = build_body(provider.kind, req);
    let request = http::Request::builder()
        .method(http::Method::POST)
        .uri(provider.chat_uri(&req.model))
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap_or_default()))
        .map_err(|e| (ProviderErrorKind::ProviderError, e.to_string()))?;
    let resp = match transport.send(request).await {
        Ok(r) => r,
        Err(e) => {
            return Err(match transport_failure(&e) {
                ProviderEvent::Failed { kind, message, .. } => (kind, message),
                _ => (ProviderErrorKind::ProviderError, String::new()),
            });
        }
    };
    if !resp.status().is_success() {
        return Err(match error_from_response(resp).await {
            ProviderEvent::Failed { kind, message, .. } => (kind, message),
            _ => (ProviderErrorKind::ProviderError, String::new()),
        });
    }
    let bytes = resp
        .into_body()
        .into_bytes()
        .await
        .map_err(|e| (ProviderErrorKind::ProviderError, e.to_string()))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| (ProviderErrorKind::ProviderError, e.to_string()))?;
    Ok(parse_completion(provider.kind, &v))
}

/// Extracts text and usage from a non-streaming response body.
#[must_use]
pub fn parse_completion(kind: ProviderKind, v: &Value) -> Completion {
    match kind {
        ProviderKind::OpenaiChatCompletions => {
            let text = v
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let usage = v.get("usage").map(|u| ProviderUsage {
                input_tokens: u.get("prompt_tokens").and_then(Value::as_i64).unwrap_or(0),
                output_tokens: u.get("completion_tokens").and_then(Value::as_i64).unwrap_or(0),
                reasoning_tokens: u
                    .get("completion_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                ..Default::default()
            });
            Completion { text, usage }
        }
        ProviderKind::AnthropicMessages => {
            let text = v
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            let usage = v.get("usage").map(|u| ProviderUsage {
                input_tokens: u.get("input_tokens").and_then(Value::as_i64).unwrap_or(0),
                output_tokens: u.get("output_tokens").and_then(Value::as_i64).unwrap_or(0),
                ..Default::default()
            });
            Completion { text, usage }
        }
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            let mut text = v.get("output_text").and_then(Value::as_str).unwrap_or_default().to_owned();
            if text.is_empty()
                && let Some(items) = v.get("output").and_then(Value::as_array)
            {
                for item in items {
                    if let Some(content) = item.get("content").and_then(Value::as_array) {
                        for part in content {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                text.push_str(t);
                            }
                        }
                    }
                }
            }
            Completion {
                text,
                usage: v.get("usage").and_then(openai_responses::parse_usage),
            }
        }
    }
}
