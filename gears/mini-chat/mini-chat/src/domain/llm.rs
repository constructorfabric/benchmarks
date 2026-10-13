//! Provider-neutral LLM port (DESIGN section 3.2 `llm_provider`, ADR-0001,
//! ADR-0002, ADR-0005).
//!
//! The domain builds an [`LlmRequest`], asks an [`LlmProviderResolver`] for
//! the [`ProviderTarget`] of the catalog model's `provider_id` and tenant, and
//! calls [`LlmProvider::stream`] (chat) or [`LlmProvider::complete`] (thread
//! summary). The adapter selected by the target's kind builds the provider
//! request, sends it through OAGW, and translates the provider wire events into
//! [`LlmEvent`]s.
//!
//! The event stream is read incrementally (no buffering): every event is
//! yielded as soon as the provider sends it. It ends right after the terminal
//! [`LlmEvent::Completed`] or [`LlmEvent::Failed`]; it ends without a terminal
//! event when the cancellation token fires. Dropping the stream, or cancelling
//! the token, aborts the provider HTTP request.
//!
//! Messages in [`LlmFailure::message_sanitized`] went through
//! [`crate::domain::sanitize::sanitize_provider_message`] and may be sent to
//! clients; raw provider identifiers never appear in them.

use std::collections::BTreeMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;
use mini_chat_sdk::ModelApiParams;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::ProviderKind;
use crate::domain::error::DomainError;

// ── Request ──────────────────────────────────────────────────────────────

/// One provider request (chat turn or thread summary).
#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    /// Provider model id (`provider_model_id` of the catalog entry).
    pub model: String,
    /// System instructions; omitted from the request when empty.
    pub instructions: String,
    /// Conversation in chronological order, the current user message last.
    pub input: Vec<LlmInputItem>,
    /// Tools offered to the model.
    pub tools: Vec<LlmTool>,
    /// Applied output token limit.
    pub max_output_tokens: u32,
    /// Catalog `max_tool_calls` (shared by all built-in tools).
    pub max_tool_calls: u32,
    /// Catalog `general_config.api_params`.
    pub api_params: ModelApiParams,
    /// Provider `user` field, see [`user_field`].
    pub user: String,
    /// Observability metadata (DESIGN "Provider Request Metadata").
    pub metadata: LlmMetadata,
    /// Secondary copies of the request's images: primary provider file id
    /// (as in [`LlmInputItem::User::image_file_ids`]) to the file id of the
    /// copy in the Anthropic Files API (`attachments.secondary_file_id`).
    /// Read only by the `anthropic_messages` adapter, which drops images
    /// without a copy (DESIGN "Known limitation — attachments after a
    /// cross-provider downgrade"). Internal only.
    pub secondary_file_ids: BTreeMap<String, String>,
}

/// One conversation item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmInputItem {
    /// User message; `image_file_ids` are provider file ids of attached images
    /// (internal only, never exposed to clients).
    User {
        text: String,
        image_file_ids: Vec<String>,
    },
    /// Earlier assistant answer.
    Assistant { text: String },
    /// A function tool call of an earlier provider iteration of the same turn
    /// (knowledge-search agentic loop), sent back as the model produced it.
    FunctionCall(LlmFunctionCall),
    /// The output the gear produced for the function call `call_id`
    /// (Responses `function_call_output`, Chat Completions `tool` message,
    /// Anthropic `tool_result`).
    FunctionCallOutput { call_id: String, output: String },
}

/// Provider tool offered to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmTool {
    /// File search over the chat's vector store.
    FileSearch {
        vector_store_id: String,
        max_num_results: u32,
    },
    /// Provider web search with the catalog `web_search_context_size`.
    WebSearch { search_context_size: String },
    /// Code interpreter over the given provider file ids.
    CodeInterpreter { file_ids: Vec<String> },
    /// Client-side function tool (e.g. `search_knowledge`). The Chat
    /// Completions, `OpenAI` Responses and Anthropic adapters send it; the vLLM
    /// adapter drops it (DESIGN section 3.2 `llm_provider`).
    Function(LlmFunctionTool),
}

/// Definition of a client-side function tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmFunctionTool {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: serde_json::Value,
}

impl LlmTool {
    /// Tool name used in `tool` events and in [`feature_label`].
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::FileSearch { .. } => TOOL_FILE_SEARCH,
            Self::WebSearch { .. } => TOOL_WEB_SEARCH,
            Self::CodeInterpreter { .. } => TOOL_CODE_INTERPRETER,
            Self::Function(_) => TOOL_FUNCTION,
        }
    }
}

/// Tool name of `file_search`.
pub const TOOL_FILE_SEARCH: &str = "file_search";
/// Tool name of `web_search`.
pub const TOOL_WEB_SEARCH: &str = "web_search";
/// Tool name of `code_interpreter`.
pub const TOOL_CODE_INTERPRETER: &str = "code_interpreter";
/// [`LlmTool::name`] of a client-side function tool (not a `metadata.feature`
/// label).
pub const TOOL_FUNCTION: &str = "function";

/// Kind of provider request, `metadata.request_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmRequestType {
    Chat,
    Summary,
    /// Defined by DESIGN; no request uses it.
    DocSummary,
}

impl LlmRequestType {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Summary => "summary",
            Self::DocSummary => "doc_summary",
        }
    }
}

/// `metadata` object of the provider request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmMetadata {
    pub tenant_id: String,
    pub user_id: String,
    pub chat_id: String,
    pub request_type: LlmRequestType,
    /// See [`feature_label`].
    pub feature: String,
}

/// Provider `user` field: both ids in simple (hyphen-less) form, tenant first,
/// 64 characters. Falls back to `{tenant_id}:{user_id}` when either id is not a
/// UUID.
#[must_use]
pub fn user_field(tenant_id: &str, user_id: &str) -> String {
    match (Uuid::parse_str(tenant_id), Uuid::parse_str(user_id)) {
        (Ok(tenant), Ok(user)) => format!("{}{}", tenant.simple(), user.simple()),
        _ => format!("{tenant_id}:{user_id}"),
    }
}

/// `metadata.feature` from the tools of a request: `none`, or the tool names
/// joined with `+` in the order `file_search`, `web_search`,
/// `code_interpreter`.
#[must_use]
pub fn feature_label(tools: &[LlmTool]) -> String {
    let names: Vec<&str> = [TOOL_FILE_SEARCH, TOOL_WEB_SEARCH, TOOL_CODE_INTERPRETER]
        .into_iter()
        .filter(|name| tools.iter().any(|tool| tool.name() == *name))
        .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join("+")
    }
}

// ── Events ───────────────────────────────────────────────────────────────

/// Internal event translated from the provider stream.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// Incremental answer text.
    TextDelta(String),
    /// Incremental reasoning text (adapters that expose it).
    ReasoningDelta(String),
    /// A tool call started; `details` is non-sensitive.
    ToolStarted {
        name: String,
        details: serde_json::Value,
    },
    /// A tool call finished; `details` is non-sensitive (e.g.
    /// `{"files_searched": n}`, `{"output": "..."}`).
    ToolCompleted {
        name: String,
        details: serde_json::Value,
    },
    /// A citation annotation (deduplicated by the adapter).
    Annotation(LlmAnnotation),
    /// The model requested a client-side function tool. The provider request
    /// ends with this tool use (the terminal event follows); the caller
    /// decides how to handle it (DESIGN "Streaming error codes"
    /// `unexpected_tool_use`).
    FunctionCall(LlmFunctionCall),
    /// Terminal: the provider completed the response. `incomplete_reason` is
    /// set for a truncated but valid response (`response.incomplete`).
    Completed {
        /// Provider response id; internal only.
        response_id: Option<String>,
        usage: Option<LlmUsage>,
        incomplete_reason: Option<String>,
        /// Full answer text.
        output_text: String,
    },
    /// Terminal: the provider failed.
    Failed(LlmFailure),
}

impl LlmEvent {
    /// Whether this event ends the stream.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed(_))
    }
}

/// A function tool call requested by the model. `call_id` is a provider
/// identifier (internal only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmFunctionCall {
    pub call_id: String,
    pub name: String,
    /// Arguments as the JSON text the model produced.
    pub arguments: String,
}

/// Citation source kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmAnnotationKind {
    /// Web page (`url_citation`).
    Url,
    /// Provider file (`file_citation`).
    File,
}

/// Citation annotation. `file_id` is a provider id (internal only; map it to
/// the attachment before exposing anything).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmAnnotation {
    pub kind: LlmAnnotationKind,
    pub url: Option<String>,
    pub title: Option<String>,
    pub file_id: Option<String>,
    pub filename: Option<String>,
    /// Character offsets into the output text part carrying the annotation,
    /// present only when the provider sent both.
    pub start: Option<usize>,
    pub end: Option<usize>,
    /// Web citations: the annotation text, or the answer text in the
    /// `start..end` range (empty when the range is outside the part). File
    /// citations: always empty.
    pub snippet: String,
}

/// Token usage reported by the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_field_names,
    reason = "field names follow the provider usage counters"
)]
pub struct LlmUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Cached input tokens read (`input_tokens_details.cached_tokens`).
    pub cache_read_input_tokens: i64,
    /// Input tokens written to the cache (Anthropic only).
    pub cache_write_input_tokens: i64,
    /// Reasoning tokens (`output_tokens_details.reasoning_tokens`).
    pub reasoning_tokens: i64,
}

impl LlmUsage {
    /// Usage is known when input or output tokens are non-zero (DESIGN
    /// section 5.7).
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }
}

/// Classification of a provider failure (streaming error code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmFailureKind {
    /// `provider_error`.
    ProviderError,
    /// `provider_timeout`.
    ProviderTimeout,
    /// `rate_limited`.
    RateLimited,
}

impl LlmFailureKind {
    /// Streaming error code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
        }
    }
}

/// Provider failure: before the stream (HTTP error) or as terminal event.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {message_sanitized}", kind.code())]
pub struct LlmFailure {
    pub kind: LlmFailureKind,
    /// Client-safe message (sanitized).
    pub message_sanitized: String,
    /// Provider error code, internal only (logs, metrics).
    pub provider_code: Option<String>,
    /// Usage of a failed response, when the provider reported it.
    pub usage: Option<LlmUsage>,
    /// Numeric `Retry-After` of a provider 429.
    pub retry_after_secs: Option<u64>,
    /// Whether the request may have left the gear. `false` only for adapter
    /// failures before any outbound call (security context, body or HTTP
    /// request build): the turn is then settled as a pre-provider failure
    /// (reserve released, zero charge; DESIGN sections 5.7-5.9) while the
    /// client still receives `provider_error`.
    pub request_sent: bool,
}

impl LlmFailure {
    /// `provider_error` with the given (already sanitized) message.
    #[must_use]
    pub fn provider_error(message_sanitized: impl Into<String>) -> Self {
        Self::new(LlmFailureKind::ProviderError, message_sanitized)
    }

    /// `provider_error` of an adapter failure before any outbound call
    /// ([`Self::request_sent`] is `false`).
    #[must_use]
    pub fn not_sent(message_sanitized: impl Into<String>) -> Self {
        Self {
            request_sent: false,
            ..Self::provider_error(message_sanitized)
        }
    }

    /// Failure of the given kind with the given (already sanitized) message.
    #[must_use]
    pub fn new(kind: LlmFailureKind, message_sanitized: impl Into<String>) -> Self {
        Self {
            kind,
            message_sanitized: message_sanitized.into(),
            provider_code: None,
            usage: None,
            retry_after_secs: None,
            request_sent: true,
        }
    }
}

/// Result of a non-streaming request (thread summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmCompletion {
    pub text: String,
    pub usage: Option<LlmUsage>,
    /// Provider response id; internal only.
    pub response_id: Option<String>,
    /// Set when the provider truncated the response.
    pub incomplete_reason: Option<String>,
}

/// Incrementally read provider event stream, see the module documentation.
pub type LlmEventStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

// ── Target and ports ─────────────────────────────────────────────────────

/// Where and how a request is sent, resolved from the catalog model's
/// `provider_id` and the tenant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTarget {
    /// `providers.<id>` key.
    pub provider_id: String,
    /// Adapter kind of the entry.
    pub kind: ProviderKind,
    /// OAGW upstream alias (tenant override alias when one applies).
    pub alias: String,
    /// Chat endpoint path; `{model}` is replaced by the provider model id.
    pub api_path: String,
    /// Tenant the target was resolved for.
    pub tenant_id: Uuid,
    /// Whether a tenant override supplied the alias.
    pub tenant_override: bool,
}

/// Maps a catalog model's `provider_id` and the tenant to a [`ProviderTarget`].
pub trait LlmProviderResolver: Send + Sync {
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the provider id is unknown or its kind
    /// has no adapter (provider resolution failure before streaming: 500).
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError>;
}

/// LLM adapter (dispatching on [`ProviderTarget::kind`]).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Start a streaming request. HTTP errors and failures before the first
    /// byte are returned as `Err`; later failures arrive as
    /// [`LlmEvent::Failed`].
    ///
    /// # Errors
    ///
    /// [`LlmFailure`] when the request could not be started.
    async fn stream(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
        cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure>;

    /// Send a non-streaming request (thread summary).
    ///
    /// # Errors
    ///
    /// [`LlmFailure`] on any provider or gateway failure, including a
    /// `failed` response.
    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure>;
}

#[cfg(test)]
#[path = "llm_tests.rs"]
mod tests;
