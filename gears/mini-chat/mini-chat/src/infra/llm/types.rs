//! Provider-agnostic LLM request/event types shared by all adapters.

use mini_chat_sdk::{ModelApiParams, WebSearchContextSize};
use serde_json::Value;

/// Role of a context message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRole {
    User,
    Assistant,
}

/// One content part of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmPart {
    Text(String),
    /// Image referenced by provider file id (`input_image.file_id`); the
    /// Anthropic Files id (secondary copy) when present.
    Image {
        file_id: String,
        secondary_file_id: Option<String>,
    },
}

/// One context message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMessage {
    pub role: LlmRole,
    pub parts: Vec<LlmPart>,
}

impl LlmMessage {
    #[must_use]
    pub fn text(role: LlmRole, text: impl Into<String>) -> Self {
        Self { role, parts: vec![LlmPart::Text(text.into())] }
    }

    /// Concatenated text of the message.
    #[must_use]
    pub fn joined_text(&self) -> String {
        let mut out = String::new();
        for p in &self.parts {
            if let LlmPart::Text(t) = p {
                out.push_str(t);
            }
        }
        out
    }
}

/// Tool sent to the provider.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmTool {
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

impl LlmTool {
    /// Feature name used in request metadata.
    #[must_use]
    pub fn feature_name(&self) -> Option<&'static str> {
        match self {
            Self::FileSearch { .. } => Some("file_search"),
            Self::WebSearch { .. } => Some("web_search"),
            Self::CodeInterpreter { .. } => Some("code_interpreter"),
            Self::Function { .. } => None,
        }
    }
}

/// Observability metadata attached to the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    /// `chat` or `summary`.
    pub request_type: &'static str,
    /// `none` or features joined with `+`.
    pub feature: String,
}

/// Provider-agnostic request.
#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub provider_model_id: String,
    /// System instructions (system prompt + tool guards); may be empty.
    pub instructions: String,
    pub messages: Vec<LlmMessage>,
    pub max_output_tokens: u32,
    pub tools: Vec<LlmTool>,
    pub max_tool_calls: u32,
    pub api_params: ModelApiParams,
    /// `{tenant_hex}{user_hex}` (64 chars).
    pub user: String,
    pub metadata: LlmMetadata,
    pub stream: bool,
    /// Extra raw input items appended after `messages` (knowledge-search
    /// agentic loop: previous function calls and their outputs).
    pub extra_input: Vec<Value>,
}

/// Provider-reported usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_field_names, reason = "field names mirror the provider usage wire terms")]
pub struct ProviderUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

impl ProviderUsage {
    /// `true` when at least one of input/output is non-zero.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }

    #[must_use]
    pub const fn total(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Raw citation annotation extracted from the provider output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u64, u64)>,
    },
    File {
        file_id: String,
        filename: Option<String>,
    },
}

/// Terminal failure classification of a provider call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureKind {
    ProviderError,
    ProviderTimeout,
    RateLimited,
}

impl ProviderFailureKind {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Internal streaming event produced by an adapter.
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
    /// A function tool call that ends the provider request (agentic loop).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        raw_item: Value,
    },
    Completed {
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
        citations: Vec<RawCitation>,
        output_text: Option<String>,
    },
    Failed {
        kind: ProviderFailureKind,
        /// Sanitized client-visible message.
        message: String,
        /// Raw provider code (internal only).
        provider_code: Option<String>,
        usage: Option<ProviderUsage>,
        response_id: Option<String>,
    },
}
