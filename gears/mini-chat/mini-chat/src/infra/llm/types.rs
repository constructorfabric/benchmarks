//! Provider-neutral request and event types used by the domain.

use mini_chat_sdk::{ModelApiParams, UsageTokens};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    Image { file_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: Role,
    pub parts: Vec<ContentPart>,
}

impl InputMessage {
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self { role, parts: vec![ContentPart::Text(text.into())] }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    FileSearch { vector_store_ids: Vec<String>, max_num_results: u32 },
    WebSearch { search_context_size: String },
    CodeInterpreter { file_ids: Vec<String> },
}

impl ToolSpec {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
        }
    }
}

/// Chat request (streaming or not).
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub max_output_tokens: u32,
    pub max_tool_calls: Option<u32>,
    pub tools: Vec<ToolSpec>,
    pub user: String,
    pub metadata: serde_json::Map<String, Value>,
    pub api_params: ModelApiParams,
    pub stream: bool,
}

/// A citation annotation as reported by the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawAnnotation {
    Url { url: String, title: String, start: Option<usize>, end: Option<usize>, part_text: Option<String> },
    File { file_id: String, filename: Option<String> },
}

/// Stable streaming error codes (SSE `error.code`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorCode {
    ProviderError,
    ProviderTimeout,
    RateLimited,
}

impl StreamErrorCode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Internal provider events produced by the adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart { name: String, details: Value },
    ToolDone { name: String, details: Value },
    Annotation(RawAnnotation),
    Completed {
        response_id: Option<String>,
        usage: Option<UsageTokens>,
        annotations: Vec<RawAnnotation>,
        incomplete_reason: Option<String>,
    },
    Failed { code: StreamErrorCode, message: String, usage: Option<UsageTokens> },
}

/// Result of a non-streaming completion (thread summary).
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    pub usage: Option<UsageTokens>,
}

/// Errors of provider calls made before a stream exists or of non-stream calls.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ProviderCallError {
    pub code: StreamErrorCode,
    pub message: String,
    pub status: Option<u16>,
    /// Context-length style failure (summary PTL retry).
    pub context_length: bool,
}
