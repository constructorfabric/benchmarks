//! Provider-agnostic request and event types of the `llm_provider` layer.

use mini_chat_sdk::ModelApiParams;

/// Role of a conversation input message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRole {
    User,
    Assistant,
}

/// One content part of an input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    /// Image referenced by the provider file id (RAG storage) and, for
    /// Anthropic chats, the secondary Anthropic Files id.
    Image {
        file_id: String,
        secondary_file_id: Option<String>,
    },
}

/// One conversation input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMessage {
    pub role: InputRole,
    pub content: Vec<ContentPart>,
}

impl InputMessage {
    #[must_use]
    pub fn text(role: InputRole, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text(text.into())],
        }
    }
}

/// A tool offered to the model.
#[derive(Debug, Clone, PartialEq)]
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
    Function {
        name: String,
        description: String,
        parameters: serde_json::Value,
    },
}

impl ToolSpec {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
            Self::Function { name, .. } => name,
        }
    }
}

/// Function-call continuation items of the knowledge-search loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionItem {
    Call {
        call_id: String,
        name: String,
        arguments: String,
    },
    Output {
        call_id: String,
        output: String,
    },
}

/// Observability metadata sent with every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    /// `chat` or `summary`.
    pub request_type: String,
    /// `none` or tools joined with `+`.
    pub feature: String,
}

/// A provider request.
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Provider model name (`provider_model_id`).
    pub model: String,
    /// System instructions (system prompt and tool guards).
    pub instructions: String,
    pub input: Vec<InputMessage>,
    pub function_items: Vec<FunctionItem>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: u32,
    pub max_tool_calls: Option<u32>,
    pub api_params: ModelApiParams,
    /// Composite tenant+user id (`user` field).
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
}

/// Provider-reported usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_field_names)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub reasoning_tokens: i64,
}

impl Usage {
    /// `true` when at least one of the main counts is non-zero.
    #[must_use]
    pub fn is_nonzero(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }
}

/// A raw citation annotation from the provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Annotation {
    File {
        file_id: String,
        filename: Option<String>,
    },
    Url {
        url: String,
        title: String,
        start: Option<usize>,
        end: Option<usize>,
        /// Text the annotation carries itself (if any).
        text: Option<String>,
        /// The `output_text` part the annotation belongs to.
        part_text: Option<String>,
    },
}

/// Classification of a provider failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// `provider_error`.
    Provider,
    /// `provider_timeout`.
    Timeout,
    /// `rate_limited` (provider 429) with the numeric `Retry-After`, if any.
    RateLimited { retry_after_secs: Option<u64> },
}

impl ProviderErrorKind {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Provider => "provider_error",
            Self::Timeout => "provider_timeout",
            Self::RateLimited { .. } => "rate_limited",
        }
    }
}

/// A provider failure (raw message: must be sanitized before reaching a
/// client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub provider_code: Option<String>,
    pub usage: Option<Usage>,
    pub response_id: Option<String>,
}

impl ProviderFailure {
    #[must_use]
    pub fn provider(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Provider,
            message: message.into(),
            provider_code: None,
            usage: None,
            response_id: None,
        }
    }

    #[must_use]
    pub fn timeout(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Timeout,
            message: message.into(),
            provider_code: None,
            usage: None,
            response_id: None,
        }
    }
}

/// Normal end of a provider response.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Completion {
    pub usage: Option<Usage>,
    pub response_id: Option<String>,
    /// Set for a provider `incomplete` response (truncated but valid).
    pub incomplete_reason: Option<String>,
}

/// A translated provider stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStart {
        name: String,
        details: serde_json::Value,
    },
    ToolDone {
        name: String,
        details: serde_json::Value,
    },
    Annotation(Annotation),
    /// The model ended the request with a function call (knowledge search
    /// loop or an unexpected tool).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    Completed(Completion),
    Failed(ProviderFailure),
}

/// Result of a non-streaming completion (thread summary).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompleteResponse {
    pub text: String,
    pub usage: Usage,
}
