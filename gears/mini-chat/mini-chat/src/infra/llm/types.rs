//! Provider-agnostic request and stream event types.

use mini_chat_sdk::{ApiParams, UsageTokens};
use serde_json::Value;

/// Role of an input message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRole {
    User,
    Assistant,
}

/// Content part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPart {
    Text(String),
    Image { file_id: String },
}

/// One message of the request input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: InputRole,
    pub parts: Vec<InputPart>,
    /// Current user message (sent as a content array).
    pub is_current: bool,
}

/// Built-in or function tool of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolSpec {
    FileSearch {
        vector_store_ids: Vec<String>,
        max_num_results: u32,
    },
    WebSearch {
        search_context_size: String,
    },
    CodeInterpreter {
        file_ids: Vec<String>,
    },
}

impl ToolSpec {
    /// Feature name of the tool (metadata `feature`).
    #[must_use]
    pub const fn feature(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
        }
    }
}

/// Provider request (chat turn or thread summary).
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub max_tool_calls: Option<u32>,
    pub user: String,
    pub metadata: serde_json::Map<String, Value>,
    pub api_params: ApiParams,
    pub stream: bool,
}

/// Citation as reported by the provider (before mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    File {
        file_id: String,
        filename: Option<String>,
    },
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u64, u64)>,
    },
}

/// Stream error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    ProviderError,
    ProviderTimeout,
    RateLimited,
}

impl ProviderErrorKind {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Terminal provider failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub kind: ProviderErrorKind,
    /// Unsanitized message (sanitize before sending to clients).
    pub message: String,
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
}

impl ProviderFailure {
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::ProviderError,
            message: message.into(),
            usage: None,
            response_id: None,
        }
    }
}

/// Internal stream event produced by an adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart { name: String, details: Value },
    ToolDone { name: String, details: Value },
    Citation(RawCitation),
    Completed {
        usage: Option<UsageTokens>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed(ProviderFailure),
}
