//! Provider-agnostic request and event types of the `llm_provider` library.

use mini_chat_sdk::{ModelApiParams, UsageTokens, WebSearchContextSize};
use serde_json::Value;

/// Message role in the provider input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Content part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    /// Image referenced by provider file id (never exposed to clients).
    Image {
        file_id: String,
        /// Anthropic Files API id of the secondary copy, when any.
        secondary_file_id: Option<String>,
    },
}

/// One provider input item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputItem {
    Message {
        role: Role,
        content: Vec<ContentPart>,
    },
    /// A function call made by the model (knowledge-search loop).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The output of a function call.
    FunctionCallOutput { call_id: String, output: String },
}

impl InputItem {
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::Message {
            role,
            content: vec![ContentPart::Text(text.into())],
        }
    }
}

/// Tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolSpec {
    FileSearch {
        vector_store_ids: Vec<String>,
        max_num_results: u32,
    },
    WebSearch {
        context_size: WebSearchContextSize,
    },
    CodeInterpreter {
        file_ids: Vec<String>,
    },
    Function {
        name: String,
        description: String,
        parameters: Value,
    },
}

impl ToolSpec {
    #[must_use]
    pub const fn feature_name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
            Self::Function { .. } => "search_knowledge",
        }
    }
}

/// Observability metadata attached to provider requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    /// `chat` | `summary`.
    pub request_type: &'static str,
    /// `none` or `+`-joined tool names.
    pub feature: String,
}

/// A provider request (streaming chat or non-streaming summary).
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<InputItem>,
    pub max_output_tokens: u32,
    pub tools: Vec<ToolSpec>,
    pub max_tool_calls: u32,
    pub api_params: ModelApiParams,
    /// `{tenant_hex}{user_hex}` (64 chars).
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
}

/// Stable streaming error codes produced by the provider layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorCode {
    ProviderError,
    ProviderTimeout,
    RateLimited,
}

impl ProviderErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Provider failure (sanitized message).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub code: ProviderErrorCode,
    pub message: String,
    pub usage: Option<UsageTokens>,
}

impl ProviderFailure {
    #[must_use]
    pub fn new(code: ProviderErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: crate::domain::sanitize::sanitize_provider_message(&message.into()),
            usage: None,
        }
    }
}

/// A citation annotation as reported by the provider (provider ids inside).
#[derive(Debug, Clone, PartialEq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(usize, usize)>,
    },
    File {
        file_id: String,
        filename: Option<String>,
        span: Option<(usize, usize)>,
    },
}

/// Internal provider stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart {
        name: String,
        details: Value,
    },
    ToolDone {
        name: String,
        details: Value,
    },
    /// The model requested a function call; ends the provider request with a
    /// tool-use outcome.
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Completed {
        response_id: Option<String>,
        usage: Option<UsageTokens>,
        citations: Vec<RawCitation>,
        /// `Some(reason)` for a provider `incomplete` response.
        incomplete_reason: Option<String>,
    },
    Failed(ProviderFailure),
}
