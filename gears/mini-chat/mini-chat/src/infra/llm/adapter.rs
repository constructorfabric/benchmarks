//! The adapter contract: request building and provider-event translation per provider API.

use std::sync::Arc;

use serde_json::Value;

use super::{anthropic, chat_completions, openai_responses, vllm_responses};
use crate::config::ProviderKind;
use crate::domain::llm::{LlmCompletion, LlmEvent, LlmRequest, ProviderError};

/// Translates the events of one provider stream. One parser per stream.
pub trait EventParser: Send {
    /// Translates one server-sent event (`event:` name when present, `data:` payload).
    fn push(&mut self, event: Option<&str>, data: &str) -> Vec<LlmEvent>;

    /// Called when the byte stream ended: events a stream end implies (for example the terminal
    /// event of Chat Completions, which has no explicit one after the usage chunk).
    fn finish(&mut self) -> Vec<LlmEvent> {
        Vec::new()
    }
}

/// A provider API: builds requests and translates responses.
pub trait Adapter: Send + Sync {
    /// The JSON request body.
    ///
    /// # Errors
    /// A [`ProviderError`] when the request cannot be expressed in this API.
    fn build_body(&self, request: &LlmRequest, stream: bool) -> Result<Value, ProviderError>;

    /// A fresh parser for one stream.
    fn parser(&self) -> Box<dyn EventParser>;

    /// Reads the answer of a non-streaming call.
    ///
    /// # Errors
    /// A [`ProviderError`] when the body has no usable answer.
    fn parse_completion(&self, body: &Value) -> Result<LlmCompletion, ProviderError>;

    /// Headers every request of this API carries besides the content type.
    fn extra_headers(&self) -> Vec<(&'static str, &'static str)> {
        Vec::new()
    }
}

/// The adapter of a provider kind.
#[must_use]
pub fn adapter_for(kind: ProviderKind) -> Arc<dyn Adapter> {
    match kind {
        ProviderKind::OpenaiResponses => Arc::new(openai_responses::OpenAiResponses),
        ProviderKind::OpenaiChatCompletions => Arc::new(chat_completions::ChatCompletions),
        ProviderKind::VllmResponses => Arc::new(vllm_responses::VllmResponses),
        ProviderKind::AnthropicMessages => Arc::new(anthropic::AnthropicMessages),
    }
}
