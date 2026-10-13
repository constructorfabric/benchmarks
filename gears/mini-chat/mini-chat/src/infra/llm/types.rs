//! Provider-neutral request, event and error types of the LLM layer
//! (DESIGN §3.2 `llm_provider`, §3.3 Provider Event Translation).

use mini_chat_sdk::ModelApiParams;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::domain::model::MessageRole;
use crate::domain::sanitize::sanitize_provider_message;

/// One provider call (streaming chat or non-streaming summary).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Provider-side model name (`provider_model_id`).
    pub model: String,
    /// System prompt plus tool guards.
    pub instructions: String,
    /// Conversation input; the last item is the current user message.
    pub input: Vec<LlmMessage>,
    pub max_output_tokens: u32,
    pub tools: Vec<LlmTool>,
    /// Catalog `max_tool_calls`; sent only with at least one built-in tool.
    pub max_tool_calls: Option<u32>,
    pub api_params: ModelApiParams,
    /// Provider `user` field (`domain::sanitize::user_field`).
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
    /// Finished function-tool rounds of the knowledge-search agentic loop,
    /// sent after `input` in order.
    pub tool_rounds: Vec<ToolRound>,
}

/// A function tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    /// Provider call id (`call_id` / `tool_use.id`); internal only.
    pub call_id: String,
    pub name: String,
    /// JSON-encoded arguments.
    pub arguments: String,
}

/// A function call and the output the gear produced for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub call: FunctionCall,
    pub output: String,
}

/// The function calls of one provider iteration with their outputs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolRound {
    pub results: Vec<ToolResult>,
}

/// One input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMessage {
    pub role: MessageRole,
    pub text: String,
    /// Provider file ids of images attached to this message.
    pub image_file_ids: Vec<String>,
}

impl LlmMessage {
    /// Text-only message.
    #[must_use]
    pub fn text(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            image_file_ids: Vec::new(),
        }
    }
}

/// Tool offered to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmTool {
    FileSearch {
        vector_store_id: String,
        max_num_results: u32,
    },
    WebSearch {
        /// `low` | `medium` | `high`.
        context_size: String,
    },
    CodeInterpreter {
        file_ids: Vec<String>,
    },
    /// Knowledge-search function tool.
    SearchKnowledge,
}

impl LlmTool {
    /// Name used in `metadata.feature` (built-in tools only).
    #[must_use]
    pub const fn feature_name(&self) -> Option<&'static str> {
        match self {
            Self::FileSearch { .. } => Some("file_search"),
            Self::WebSearch { .. } => Some("web_search"),
            Self::CodeInterpreter { .. } => Some("code_interpreter"),
            Self::SearchKnowledge => None,
        }
    }

    /// Provider built-in tool (bounded by `max_tool_calls`).
    #[must_use]
    pub const fn is_builtin(&self) -> bool {
        self.feature_name().is_some()
    }
}

/// `metadata.request_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestType {
    Chat,
    Summary,
}

impl RequestType {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Summary => "summary",
        }
    }
}

/// Provider request metadata (DESIGN §4 "Provider Request Metadata").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub request_type: RequestType,
    /// `none` or the built-in tool names in the order `file_search`,
    /// `web_search`, `code_interpreter` joined by `+`.
    pub feature: String,
}

impl RequestMetadata {
    /// Metadata whose `feature` is derived from `tools`.
    #[must_use]
    pub fn new(
        tenant_id: Uuid,
        user_id: Uuid,
        chat_id: Uuid,
        request_type: RequestType,
        tools: &[LlmTool],
    ) -> Self {
        Self {
            tenant_id,
            user_id,
            chat_id,
            request_type,
            feature: feature_of(tools),
        }
    }
}

/// `metadata.feature` for `tools`.
#[must_use]
pub fn feature_of(tools: &[LlmTool]) -> String {
    let mut names = Vec::new();
    for wanted in ["file_search", "web_search", "code_interpreter"] {
        if tools.iter().any(|t| t.feature_name() == Some(wanted)) {
            names.push(wanted);
        }
    }
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join("+")
    }
}

/// Token usage reported by the provider.
#[allow(clippy::struct_field_names, reason = "provider usage field names")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LlmUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

/// Provider annotation before mapping to the public citation shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawCitation {
    Web {
        url: String,
        title: String,
        snippet: String,
        span: Option<(u32, u32)>,
    },
    File {
        provider_file_id: String,
        span: Option<(u32, u32)>,
    },
}

/// Internal event translated from the provider stream.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
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
    Citation(RawCitation),
    /// A complete function tool call; the iteration ends with `Completed`.
    FunctionCall(FunctionCall),
    /// `response.completed` / `response.incomplete` (with `incomplete_reason`).
    Completed {
        usage: Option<LlmUsage>,
        response_id: Option<String>,
        incomplete_reason: Option<String>,
    },
    Failed {
        error: LlmError,
        usage: Option<LlmUsage>,
    },
}

impl LlmEvent {
    /// `Completed` or `Failed`.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }
}

/// Provider call failure. Messages are internal (unsanitized); clients get
/// [`LlmError::client_message`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LlmError {
    #[error("provider rate limited: {message}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        message: String,
    },
    #[error("provider timeout: {0}")]
    Timeout(String),
    #[error("provider error: {message}")]
    Provider { message: String },
    #[error("provider unavailable: {0}")]
    Unavailable(String),
}

impl LlmError {
    /// Streaming error code (DESIGN §3.3 "Streaming error codes").
    #[must_use]
    pub const fn sse_code(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate_limited",
            Self::Timeout(_) => "provider_timeout",
            Self::Provider { .. } | Self::Unavailable(_) => "provider_error",
        }
    }

    /// Sanitized client-visible message; `rate_limited` adds `retry in {N}s`
    /// when the provider sent a numeric `Retry-After`.
    #[must_use]
    pub fn client_message(&self) -> String {
        let (text, fallback) = match self {
            Self::RateLimited { message, .. } => (message, "provider rate limit exceeded"),
            Self::Timeout(message) => (message, "provider request timed out"),
            Self::Provider { message } => (message, "provider error"),
            Self::Unavailable(message) => (message, "provider unavailable"),
        };
        let text = sanitize_provider_message(text.trim());
        let mut msg = if text.is_empty() {
            fallback.to_owned()
        } else {
            text
        };
        if let Self::RateLimited {
            retry_after_secs: Some(secs),
            ..
        } = self
        {
            msg = format!("{msg}; retry in {secs}s");
        }
        msg
    }
}
