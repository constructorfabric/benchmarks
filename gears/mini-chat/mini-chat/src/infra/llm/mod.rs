//! LLM provider layer (ADR-0001, ADR-0005): provider resolution, adapters, storage.

pub mod anthropic;
pub mod chat_completions;
pub mod client;
pub mod gateway;
pub mod resolver;
pub mod responses;
pub mod sse;
pub mod storage;

use mini_chat_sdk::{ApiParams, UsageTokens, WebSearchContextSize};
use serde_json::Value;

/// A content part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    /// Text.
    Text(String),
    /// Image by provider file id (RAG provider), with an optional Anthropic copy.
    Image { file_id: String, secondary_file_id: Option<String> },
}

/// Role of an input message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// User.
    User,
    /// Assistant.
    Assistant,
}

impl Role {
    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    /// Role.
    pub role: Role,
    /// Content parts.
    pub parts: Vec<ContentPart>,
}

impl InputMessage {
    /// Plain text message.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self { role, parts: vec![ContentPart::Text(text.into())] }
    }
}

/// A built-in tool sent with the request.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolSpec {
    /// `file_search` over the chat vector store.
    FileSearch { vector_store_ids: Vec<String>, max_num_results: u32 },
    /// `web_search`.
    WebSearch { context_size: WebSearchContextSize },
    /// `code_interpreter` with container files.
    CodeInterpreter { file_ids: Vec<String> },
}

impl ToolSpec {
    /// Tool name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
        }
    }
}

/// Normalized chat request built by the domain layer.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    /// Provider model name.
    pub model: String,
    /// System instructions (system prompt plus tool guards).
    pub instructions: String,
    /// Conversation input.
    pub input: Vec<InputMessage>,
    /// Output cap.
    pub max_output_tokens: u32,
    /// Built-in tools.
    pub tools: Vec<ToolSpec>,
    /// Built-in tool calls per request (`OpenAI` Responses only).
    pub max_tool_calls: u32,
    /// `user` field.
    pub user: String,
    /// `metadata` object (`OpenAI` Responses only).
    pub metadata: serde_json::Map<String, Value>,
    /// Optional catalog parameters.
    pub api_params: ApiParams,
    /// Streaming.
    pub stream: bool,
}

/// Provider error codes surfaced over SSE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorCode {
    /// `provider_error`.
    ProviderError,
    /// `provider_timeout`.
    ProviderTimeout,
    /// `rate_limited`.
    RateLimited,
}

impl ProviderErrorCode {
    /// Wire code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Provider failure (message already sanitized).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// Code.
    pub code: ProviderErrorCode,
    /// Sanitized, client-safe message.
    pub message: String,
    /// Usage reported with the failure.
    pub usage: Option<UsageTokens>,
}

impl ProviderError {
    /// `provider_error` with a message.
    pub fn provider(msg: impl Into<String>) -> Self {
        Self {
            code: ProviderErrorCode::ProviderError,
            message: crate::domain::sanitize::sanitize_provider_message(&msg.into()),
            usage: None,
        }
    }
}

/// Raw citation from provider annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    /// File citation (provider file id).
    File { file_id: String },
    /// Web citation.
    Web { url: String, title: String, snippet: String, span: Option<(usize, usize)> },
}

/// Internal streaming events produced by adapters.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// Output text.
    TextDelta(String),
    /// Reasoning text (vLLM `<think>` blocks).
    ReasoningDelta(String),
    /// Tool started.
    ToolStart { name: String, details: Value },
    /// Tool finished.
    ToolDone { name: String, details: Value },
    /// Terminal success (`incomplete_reason` set for a truncated response).
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
        citations: Vec<RawCitation>,
    },
    /// Terminal failure.
    Failed(ProviderError),
}

/// Result of a non-streaming completion (thread summary).
#[derive(Debug, Clone)]
pub struct Completion {
    /// Output text.
    pub text: String,
    /// Usage.
    pub usage: Option<UsageTokens>,
}
