//! Provider adapters, selected by the provider entry `kind` (ADR-0005).

pub mod anthropic;
pub mod chat_completions;
pub mod openai_responses;
pub mod think;

use mini_chat_sdk::UsageTokens;
use serde_json::Value;

use crate::config::ProviderKind;
use crate::infra::llm::types::{LlmRequest, ProviderEvent};

/// Build the provider request body for an adapter kind.
#[must_use]
pub fn build_body(kind: ProviderKind, req: &LlmRequest) -> Value {
    match kind {
        ProviderKind::OpenaiResponses => openai_responses::build_body(req, true, true),
        // vLLM drops all tools (including function tools) and the metadata.
        ProviderKind::VllmResponses => openai_responses::build_body(req, false, false),
        ProviderKind::OpenaiChatCompletions => chat_completions::build_body(req),
        ProviderKind::AnthropicMessages => anthropic::build_body(req),
    }
}

/// Streaming parser of an adapter kind.
#[derive(Debug)]
pub enum StreamParser {
    Responses(openai_responses::ResponsesParser),
    Chat(chat_completions::ChatCompletionsParser),
    Anthropic(anthropic::AnthropicParser),
}

impl StreamParser {
    #[must_use]
    pub fn new(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenaiResponses => {
                Self::Responses(openai_responses::ResponsesParser::new(false))
            }
            ProviderKind::VllmResponses => {
                Self::Responses(openai_responses::ResponsesParser::new(true))
            }
            ProviderKind::OpenaiChatCompletions => {
                Self::Chat(chat_completions::ChatCompletionsParser::default())
            }
            ProviderKind::AnthropicMessages => {
                Self::Anthropic(anthropic::AnthropicParser::default())
            }
        }
    }

    pub fn push(&mut self, event: Option<&str>, data: &str) -> Vec<ProviderEvent> {
        match self {
            Self::Responses(p) => p.push(event, data),
            Self::Chat(p) => p.push(event, data),
            Self::Anthropic(p) => p.push(event, data),
        }
    }

    /// Events produced at end of stream (no terminal event seen).
    pub fn eof(&mut self) -> Vec<ProviderEvent> {
        match self {
            Self::Chat(p) => p.eof(),
            Self::Responses(_) | Self::Anthropic(_) => vec![],
        }
    }
}

/// Text and usage of a non-streaming response.
#[must_use]
pub fn parse_nonstream(kind: ProviderKind, body: &Value) -> (String, Option<UsageTokens>) {
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => (
            openai_responses::output_text(body),
            openai_responses::parse_usage(body.get("usage")),
        ),
        ProviderKind::OpenaiChatCompletions => chat_completions::parse_completion(body),
        ProviderKind::AnthropicMessages => anthropic::parse_message(body),
    }
}
