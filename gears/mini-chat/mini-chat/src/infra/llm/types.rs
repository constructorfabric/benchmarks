//! Provider-neutral LLM request / event types (`llm_provider`, D§3.2, S§9).
//!
//! Adapters translate [`LlmRequest`] into their wire body and their wire
//! events into [`LlmEvent`]s. Nothing here is client-visible: the stream
//! service maps these events to the public SSE contract.

use mini_chat_sdk::{ApiParams, UsageTokens};
use serde_json::Value;
use uuid::Uuid;

/// `metadata.request_type` of chat turns.
pub const REQUEST_TYPE_CHAT: &str = "chat";
/// `metadata.request_type` of the thread summary call.
pub const REQUEST_TYPE_SUMMARY: &str = "summary";
/// `metadata.feature` when the request carries no built-in tool.
pub const FEATURE_NONE: &str = "none";

/// One provider request (streaming chat or the non-streaming summary call).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Provider model id (`provider_model_id` of the catalog entry).
    pub model: String,
    /// System prompt plus tool guards.
    pub instructions: String,
    /// Conversation, oldest first; the last item is the current user message.
    pub input: Vec<InputMessage>,
    pub max_output_tokens: u32,
    pub tools: Vec<ToolSpec>,
    /// Catalog `max_tool_calls`.
    pub max_tool_calls: u32,
    /// Catalog `general_config.api_params`.
    pub api_params: ApiParams,
    /// Provider `user` field (see [`super::sanitize::provider_user_field`]).
    pub user: String,
    pub metadata: RequestMetadata,
    pub stream: bool,
}

/// Message author.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    /// Wire role name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One conversation message.
#[derive(Debug, Clone, PartialEq)]
pub struct InputMessage {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl InputMessage {
    /// Text-only message.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text(text.into())],
        }
    }
}

/// Part of a message.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    Text(String),
    /// Image uploaded to the provider (`provider_file_id`; the Anthropic
    /// secondary file id for Anthropic requests).
    Image {
        file_id: String,
    },
    /// A function call of the model, replayed in the next request of the
    /// knowledge-search loop (assistant message).
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    /// The output of a function call (user message).
    FunctionOutput {
        call_id: String,
        output: String,
    },
}

/// Tool offered to the model (decided by the domain before the adapter runs).
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
        parameters: Value,
    },
}

/// Observability metadata attached to every provider request
/// (D "Provider Request Metadata").
#[derive(Debug, Clone, PartialEq)]
pub struct RequestMetadata {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    /// [`REQUEST_TYPE_CHAT`] or [`REQUEST_TYPE_SUMMARY`].
    pub request_type: &'static str,
    /// `none`, or the built-in tools joined with `+`.
    pub feature: String,
}

impl RequestMetadata {
    /// Metadata of a chat turn: `feature` is derived from `tools`.
    #[must_use]
    pub fn chat(tenant_id: Uuid, user_id: Uuid, chat_id: Uuid, tools: &[ToolSpec]) -> Self {
        Self {
            tenant_id,
            user_id,
            chat_id,
            request_type: REQUEST_TYPE_CHAT,
            feature: feature_label(tools),
        }
    }

    /// Metadata of the thread summary call (`feature = none`).
    #[must_use]
    pub fn summary(tenant_id: Uuid, system_user_id: Uuid, chat_id: Uuid) -> Self {
        Self {
            tenant_id,
            user_id: system_user_id,
            chat_id,
            request_type: REQUEST_TYPE_SUMMARY,
            feature: FEATURE_NONE.to_owned(),
        }
    }
}

/// `metadata.feature`: built-in tools in the fixed order
/// `file_search`, `web_search`, `code_interpreter`, joined with `+`;
/// `none` when there is none (function tools do not count).
#[must_use]
pub fn feature_label(tools: &[ToolSpec]) -> String {
    let has = |f: fn(&ToolSpec) -> bool| tools.iter().any(f);
    let mut parts = Vec::new();
    if has(|t| matches!(t, ToolSpec::FileSearch { .. })) {
        parts.push("file_search");
    }
    if has(|t| matches!(t, ToolSpec::WebSearch { .. })) {
        parts.push("web_search");
    }
    if has(|t| matches!(t, ToolSpec::CodeInterpreter { .. })) {
        parts.push("code_interpreter");
    }
    if parts.is_empty() {
        FEATURE_NONE.to_owned()
    } else {
        parts.join("+")
    }
}

/// Internal stream event produced by an adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Built-in tool started (`file_search`, `web_search`, `code_interpreter`).
    ToolStart {
        name: String,
        details: Value,
    },
    ToolDone {
        name: String,
        details: Value,
    },
    Citation(RawCitation),
    Completed(LlmTerminal),
    /// Truncated but valid completion (finalized as completed).
    Incomplete {
        terminal: LlmTerminal,
        reason: String,
    },
    Failed(ProviderFailure),
    /// The model requested a function tool.
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
}

impl LlmEvent {
    /// Whether this event ends the provider stream.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed(_) | Self::Incomplete { .. } | Self::Failed(_)
        )
    }
}

/// Terminal data of a completed / incomplete response.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LlmTerminal {
    pub usage: Option<UsageTokens>,
    /// Provider response id (persisted internally, never exposed).
    pub response_id: Option<String>,
}

/// Citation as reported by the provider (mapped to the public items later).
#[derive(Debug, Clone, PartialEq)]
pub enum RawCitation {
    Url {
        url: String,
        title: String,
        /// Character range in the answer text, when reported.
        start: Option<usize>,
        end: Option<usize>,
        snippet: String,
    },
    File {
        provider_file_id: String,
        filename: String,
    },
}

/// Provider failure; `message` is already sanitized (client-visible).
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderFailure {
    pub code: StreamErrorCode,
    pub message: String,
    /// Usage reported with a failed response, when present.
    pub usage: Option<UsageTokens>,
}

impl ProviderFailure {
    /// Failure without usage.
    #[must_use]
    pub fn new(code: StreamErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            usage: None,
        }
    }
}

/// Provider-related streaming error codes (D "Streaming error codes").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorCode {
    ProviderError,
    ProviderTimeout,
    RateLimited,
}

impl StreamErrorCode {
    /// Wire code of SSE `event: error`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Result of a non-streaming call (thread summary).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmCompletion {
    pub text: String,
    pub usage: Option<UsageTokens>,
    pub response_id: Option<String>,
}
