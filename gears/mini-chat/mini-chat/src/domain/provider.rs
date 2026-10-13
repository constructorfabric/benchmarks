//! Provider-facing domain types: the internal event model every `llm_provider`
//! adapter maps its wire events to (DESIGN 3.3 "Provider Event Translation"),
//! the adapter-agnostic chat request and the provider failure taxonomy.
//!
//! The adapters themselves live in `infra::llm`; the stream service only knows
//! these types and the [`LlmGateway`](crate::domain::ports::LlmGateway) port.

use std::pin::Pin;

use futures::Stream;
use mini_chat_sdk::{ModelApiParams, WebSearchContextSize};
use uuid::Uuid;

use crate::domain::context::HistoryRole;

/// One provider event after translation.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// Incremental answer text.
    TextDelta(String),
    /// A built-in tool started (`file_search`, `web_search`, `code_interpreter`).
    ToolStarted {
        /// Tool name.
        name: String,
    },
    /// A built-in tool finished.
    ToolDone {
        /// Tool name.
        name: String,
        /// Tool specific, non-sensitive metadata.
        details: serde_json::Value,
    },
    /// Terminal: `response.completed` or `response.incomplete`.
    Completed(Completion),
    /// Terminal: `response.failed` or an `error` event.
    Failed(Failure),
    /// Provider noise that produces no client event.
    Ignored,
}

/// The detailed token breakdown the provider reported with its usage (DESIGN
/// 5.5.9 "Detailed token breakdown"). Both counts are subsets of the input and
/// output totals, never additive to them; absent counts are 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenBreakdown {
    /// Input tokens served from the provider cache
    /// (`input_tokens_details.cached_tokens`).
    pub cache_read: u64,
    /// Input tokens written to the provider cache (not reported by the `OpenAI`
    /// Responses API; reserved for other providers).
    pub cache_write: u64,
    /// Output tokens spent on reasoning
    /// (`output_tokens_details.reasoning_tokens`).
    pub reasoning: u64,
}

impl TokenBreakdown {
    /// No breakdown reported.
    pub const NONE: Self = Self {
        cache_read: 0,
        cache_write: 0,
        reasoning: 0,
    };
}

/// Terminal data of a completed (or incomplete) response.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Completion {
    /// Provider response id (internal only).
    pub response_id: Option<String>,
    /// The response was truncated (`response.incomplete`).
    pub incomplete: bool,
    /// Provider reported input tokens (0 when absent).
    pub input_tokens: u64,
    /// Provider reported output tokens (0 when absent).
    pub output_tokens: u64,
    /// Cache / reasoning breakdown of the reported usage.
    pub breakdown: TokenBreakdown,
    /// Citations in provider order.
    pub annotations: Vec<Annotation>,
}

/// A citation annotation of the final answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Annotation {
    /// `url_citation`.
    Web {
        /// Page URL.
        url: String,
        /// Page title.
        title: String,
        /// Annotation text, or the answer text inside the annotation range.
        snippet: String,
        /// Character range in the answer text, when the provider sent one.
        span: Option<(u64, u64)>,
    },
    /// `file_citation`.
    File {
        /// Provider file id (internal only).
        file_id: String,
        /// Provider side file name.
        filename: String,
    },
}

/// A provider reported failure; `message` is the RAW provider text.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Failure {
    /// Provider error code.
    pub code: Option<String>,
    /// Provider error message (must be sanitized before a client sees it).
    pub message: String,
    /// Usage reported with the failure, when any.
    pub input_tokens: u64,
    /// Usage reported with the failure, when any.
    pub output_tokens: u64,
    /// Cache / reasoning breakdown of the usage reported with the failure.
    pub breakdown: TokenBreakdown,
}

/// Class of a provider failure; selects the streaming error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// `provider_error`.
    Error,
    /// `provider_timeout`.
    Timeout,
    /// `rate_limited`.
    RateLimited,
}

impl ProviderErrorKind {
    /// The stable streaming error code stored in `chat_turns.error_code` and
    /// sent in the SSE `error` event.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Error => "provider_error",
            Self::Timeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// A failed provider call or stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    /// Failure class.
    pub kind: ProviderErrorKind,
    /// Message for the client. Adapters pass provider text through the
    /// sanitizer; the stream service sanitizes again before sending.
    pub message: String,
    /// The provider rejected the request because the prompt is larger than the
    /// model's context window (HTTP 400, `error.code = context_length_exceeded`).
    /// Only the thread-summary worker acts on it.
    pub context_length_exceeded: bool,
}

impl ProviderFailure {
    /// A `provider_error` with `message`.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            kind: ProviderErrorKind::Error,
            message: message.into(),
            context_length_exceeded: false,
        }
    }
}

/// Stream of translated provider events. Dropping it cancels the provider
/// request (hard cancel, DESIGN 3.6 "Streaming Cancellation").
pub type ProviderEventStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderFailure>> + Send>>;

/// A tool of one provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTool {
    /// Retrieval over the chat vector store, without metadata filters.
    FileSearch {
        /// Vector store ids (internal only).
        vector_store_ids: Vec<String>,
        /// Catalog `max_num_results`.
        max_num_results: u32,
    },
    /// Provider web search.
    WebSearch {
        /// Catalog `web_search_context_size`.
        context_size: WebSearchContextSize,
    },
    /// Code interpreter over the ready spreadsheets of the chat.
    CodeInterpreter {
        /// Provider file ids offered to the container (internal only).
        file_ids: Vec<String>,
    },
}

impl ChatTool {
    /// Label of the tool in the provider `metadata.feature` value.
    #[must_use]
    pub const fn feature(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => "file_search",
            Self::WebSearch { .. } => "web_search",
            Self::CodeInterpreter { .. } => "code_interpreter",
        }
    }
}

/// One conversation item of a provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInputItem {
    /// Author role.
    pub role: HistoryRole,
    /// Text.
    pub content: String,
    /// Provider file ids of the images sent with a user message, in order
    /// (internal only; always empty for history items).
    pub images: Vec<String>,
}

/// Adapter-agnostic chat request; built by the stream service from the
/// effective model's catalog entry and the planned context.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    /// Tenant of the caller (also selects a tenant override of the provider).
    pub tenant_id: Uuid,
    /// The caller.
    pub user_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// `providers.<id>` key of the catalog entry.
    pub provider_id: String,
    /// Model name on the provider side.
    pub provider_model_id: String,
    /// System prompt including the tool guards.
    pub instructions: String,
    /// Kept history followed by the current user message.
    pub input: Vec<ChatInputItem>,
    /// Tools of the request.
    pub tools: Vec<ChatTool>,
    /// `min(catalog max_output_tokens, streaming.max_output_tokens)`.
    pub max_output_tokens: u32,
    /// Catalog `max_tool_calls`.
    pub max_tool_calls: u32,
    /// Catalog sampling and provider parameters.
    pub api_params: ModelApiParams,
    /// The request is a thread-summary system task (`metadata.request_type =
    /// "summary"`), not a chat turn.
    pub is_summary: bool,
}
