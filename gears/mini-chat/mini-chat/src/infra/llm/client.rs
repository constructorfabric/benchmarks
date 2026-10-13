//! Streaming chat / non-streaming completion through OAGW, dispatched by adapter kind.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, StreamExt};
use serde_json::Value;

use super::anthropic::{self, AnthropicTranslator};
use super::chat_completions::{self, ChatTranslator};
use super::gateway::{BufferedResponse, Gateway, ProxyFailure, json_request};
use super::resolver::{ChatTarget, ProviderResolver};
use super::responses::{self, Flavor, ResponsesTranslator, parse_error_message};
use super::sse::SseParser;
use super::{Completion, LlmRequest, ProviderError, ProviderErrorCode, ProviderEvent};
use crate::config::ProviderKind;
use crate::domain::sanitize::sanitize_provider_message;

/// Stream of translated provider events. Ends after the first terminal event.
pub type EventStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

/// Timeout of non-streaming provider calls.
pub const COMPLETION_TIMEOUT: Duration = Duration::from_secs(240);

enum Translator {
    Responses(ResponsesTranslator),
    Chat(ChatTranslator),
    Anthropic(AnthropicTranslator),
}

impl Translator {
    fn new(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenaiResponses => Self::Responses(ResponsesTranslator::new(Flavor::OpenAi)),
            ProviderKind::VllmResponses => Self::Responses(ResponsesTranslator::new(Flavor::Vllm)),
            ProviderKind::OpenaiChatCompletions => Self::Chat(ChatTranslator::default()),
            ProviderKind::AnthropicMessages => Self::Anthropic(AnthropicTranslator::default()),
        }
    }

    fn on_frame(&mut self, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        match self {
            Self::Responses(t) => t.on_frame(event, data),
            Self::Chat(t) => t.on_frame(data),
            Self::Anthropic(t) => t.on_frame(event, data),
        }
    }
}

/// Builds the provider body for an adapter kind.
#[must_use]
pub fn build_body(kind: ProviderKind, req: &LlmRequest) -> Value {
    match kind {
        ProviderKind::OpenaiResponses => responses::build_request(req, Flavor::OpenAi),
        ProviderKind::VllmResponses => responses::build_request(req, Flavor::Vllm),
        ProviderKind::OpenaiChatCompletions => chat_completions::build_request(req),
        ProviderKind::AnthropicMessages => anthropic::build_request(req),
    }
}

fn map_failure(f: ProxyFailure) -> ProviderError {
    match f {
        ProxyFailure::NotReady => ProviderError::provider("Provider is currently unavailable"),
        ProxyFailure::Timeout(_) => ProviderError {
            code: ProviderErrorCode::ProviderTimeout,
            message: "Provider request timed out".to_owned(),
            usage: None,
        },
        ProxyFailure::Gateway(m) => {
            tracing::warn!(error = %m, "provider request failed at the gateway");
            ProviderError::provider("Provider is currently unavailable")
        }
    }
}

/// Maps a non-2xx provider response.
#[must_use]
pub fn map_status_error(resp: &BufferedResponse) -> ProviderError {
    let status = resp.status.as_u16();
    if status == 429 {
        let retry = resp
            .headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let message = match retry {
            Some(n) => format!("Provider rate limit exceeded, retry in {n}s"),
            None => "Provider rate limit exceeded".to_owned(),
        };
        return ProviderError { code: ProviderErrorCode::RateLimited, message, usage: None };
    }
    if resp.from_gateway && status == 504 {
        return ProviderError {
            code: ProviderErrorCode::ProviderTimeout,
            message: "Provider request timed out".to_owned(),
            usage: None,
        };
    }
    let msg = resp
        .json()
        .as_ref()
        .and_then(parse_error_message)
        .unwrap_or_else(|| format!("Provider returned HTTP {status}"));
    ProviderError {
        code: ProviderErrorCode::ProviderError,
        message: sanitize_provider_message(&msg),
        usage: None,
    }
}

/// LLM client.
pub struct LlmClient {
    /// OAGW access.
    pub gateway: Arc<Gateway>,
    /// Provider resolver.
    pub resolver: Arc<ProviderResolver>,
}

impl LlmClient {
    /// New client.
    #[must_use]
    pub fn new(gateway: Arc<Gateway>, resolver: Arc<ProviderResolver>) -> Self {
        Self { gateway, resolver }
    }

    /// Starts a streaming chat request.
    ///
    /// # Errors
    /// Provider error before the stream starts.
    pub async fn stream_chat(&self, target: &ChatTarget, req: &LlmRequest) -> Result<EventStream, ProviderError> {
        let body = build_body(target.kind, req);
        let http_req = json_request(http::Method::POST, &target.uri(&req.model), &body);
        let resp = self.gateway.send(http_req).await.map_err(map_failure)?;
        if !resp.status().is_success() {
            let from_gateway = resp.extensions().get::<oagw_sdk::api::ErrorSource>().copied()
                == Some(oagw_sdk::api::ErrorSource::Gateway);
            let status = resp.status();
            let headers = resp.headers().clone();
            let body = tokio::time::timeout(Duration::from_secs(30), resp.into_body().into_bytes())
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            return Err(map_status_error(&BufferedResponse { status, headers, body, from_gateway }));
        }
        let mut translator = Translator::new(target.kind);
        let mut body = resp.into_body().into_stream();
        let stream = async_stream::stream! {
            let mut parser = SseParser::default();
            let mut terminal = false;
            'outer: while let Some(chunk) = body.next().await {
                match chunk {
                    Ok(bytes) => {
                        for frame in parser.feed(&bytes) {
                            for ev in translator.on_frame(frame.event.as_deref(), &frame.data) {
                                let is_terminal = matches!(ev, ProviderEvent::Completed { .. } | ProviderEvent::Failed(_));
                                yield ev;
                                if is_terminal {
                                    terminal = true;
                                    break 'outer;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "provider stream failed");
                        yield ProviderEvent::Failed(ProviderError::provider("Provider stream failed"));
                        terminal = true;
                        break;
                    }
                }
            }
            if !terminal
                && let Some(frame) = parser.finish()
            {
                for ev in translator.on_frame(frame.event.as_deref(), &frame.data) {
                    let is_terminal = matches!(ev, ProviderEvent::Completed { .. } | ProviderEvent::Failed(_));
                    yield ev;
                    if is_terminal {
                        terminal = true;
                        break;
                    }
                }
            }
            if !terminal {
                yield ProviderEvent::Failed(ProviderError::provider("Provider stream ended without a terminal event"));
            }
        };
        Ok(Box::pin(stream))
    }

    /// Non-streaming completion (thread summary).
    ///
    /// # Errors
    /// Provider failure.
    pub async fn complete(&self, target: &ChatTarget, req: &LlmRequest) -> Result<Completion, ProviderError> {
        let mut req = req.clone();
        req.stream = false;
        let body = build_body(target.kind, &req);
        let http_req = json_request(http::Method::POST, &target.uri(&req.model), &body);
        let resp = self
            .gateway
            .send_buffered(http_req, COMPLETION_TIMEOUT)
            .await
            .map_err(map_failure)?;
        if !resp.status.is_success() {
            return Err(map_status_error(&resp));
        }
        let json = resp.json().unwrap_or(Value::Null);
        let (text, usage) = match target.kind {
            ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => responses::parse_completion(&json),
            ProviderKind::OpenaiChatCompletions => chat_completions::parse_completion(&json),
            ProviderKind::AnthropicMessages => anthropic::parse_completion(&json),
        };
        Ok(Completion { text, usage })
    }
}
