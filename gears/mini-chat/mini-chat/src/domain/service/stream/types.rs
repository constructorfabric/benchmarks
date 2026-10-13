//! Shared vocabulary of the streaming-turns feature
//! (`cpt-cf-mini-chat-feature-streaming-turns`): turn states, the SSE events
//! as the domain sees them, the internal provider events every adapter
//! produces, and the stream error codes.
//!
//! Nothing here is serialized: the wire shapes are the DTOs of
//! `api::rest::dto`, built by `api::rest::sse`.

use mini_chat_sdk::UsageTokens;
use serde_json::Value;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::service::quota::preflight::{DowngradeReason, QuotaDecision};
use crate::domain::service::quota::status::QuotaWarningView;

/// State of a `chat_turns` row.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// The turn is in progress.
    Running,
    /// The provider completed (or the response was incomplete) and the
    /// assistant message is committed.
    Completed,
    /// The turn failed.
    Failed,
    /// The client disconnected before the provider's terminal event.
    Cancelled,
}

impl TurnState {
    /// `chat_turns.state` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a stored state.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// `TurnStatusResponse.state`: `running`, `done`, `error` or `cancelled`.
    #[must_use]
    pub const fn api_state(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A `chat_turns` row as the stream pipeline reads it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRecord {
    /// Turn id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Idempotency key.
    pub request_id: Uuid,
    /// Requesting user, when set.
    pub requester_user_id: Option<Uuid>,
    /// State.
    pub state: TurnState,
    /// Persisted assistant message, when any.
    pub assistant_message_id: Option<Uuid>,
    /// Stored error code, when any.
    pub error_code: Option<String>,
    /// Effective model of the turn, when set.
    pub effective_model: Option<String>,
    /// Start time.
    pub started_at: OffsetDateTime,
    /// Last update time.
    pub updated_at: OffsetDateTime,
    /// `deleted_at IS NOT NULL`.
    pub deleted: bool,
}

/// Streaming error codes sent in the SSE `error` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StreamErrorCode {
    /// Provider error, invalid response, unavailable or failed stream.
    ProviderError,
    /// Gateway timeout or the gateway's own `deadline_exceeded`.
    ProviderTimeout,
    /// Provider HTTP 429.
    RateLimited,
    /// More `web_search` starts than the per-message limit.
    WebSearchCallsExceeded,
    /// More `code_interpreter` starts than the per-message limit.
    CodeInterpreterCallsExceeded,
    /// Knowledge-search loop iterations exceeded.
    AgenticIterationsExceeded,
    /// A function tool the gear does not handle.
    UnexpectedToolUse,
    /// The assistant message could not be persisted.
    MessagePersistenceFailed,
    /// The finalization transaction failed.
    FinalizationFailed,
    /// The provider task ended without a terminal event.
    StreamInterrupted,
}

impl StreamErrorCode {
    /// Wire value and `chat_turns.error_code`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProviderError => "provider_error",
            Self::ProviderTimeout => "provider_timeout",
            Self::RateLimited => "rate_limited",
            Self::WebSearchCallsExceeded => "web_search_calls_exceeded",
            Self::CodeInterpreterCallsExceeded => "code_interpreter_calls_exceeded",
            Self::AgenticIterationsExceeded => "agentic_iterations_exceeded",
            Self::UnexpectedToolUse => "unexpected_tool_use",
            Self::MessagePersistenceFailed => "message_persistence_failed",
            Self::FinalizationFailed => "finalization_failed",
            Self::StreamInterrupted => "stream_interrupted",
        }
    }

    /// The generic client text of the code (used when there is no provider
    /// message).
    #[must_use]
    pub const fn generic_message(self) -> &'static str {
        match self {
            Self::ProviderError => "The provider is unavailable or returned an error.",
            Self::ProviderTimeout => "The provider did not answer in time.",
            Self::RateLimited => "The provider rate limit was reached.",
            Self::WebSearchCallsExceeded => "The web search call limit of the turn was exceeded.",
            Self::CodeInterpreterCallsExceeded => {
                "The code interpreter call limit of the turn was exceeded."
            }
            Self::AgenticIterationsExceeded => "The tool iteration limit of the turn was exceeded.",
            Self::UnexpectedToolUse => "The model requested a tool that is not available.",
            Self::MessagePersistenceFailed => "The answer could not be saved.",
            Self::FinalizationFailed => "The turn could not be finalized.",
            Self::StreamInterrupted => "The stream was interrupted.",
        }
    }
}

/// Tool names of the SSE `tool` event.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolName {
    /// `file_search`
    FileSearch,
    /// `web_search`
    WebSearch,
    /// `code_interpreter`
    CodeInterpreter,
    /// `function_call`
    FunctionCall,
    /// `search_knowledge`
    SearchKnowledge,
    /// `load_files`
    LoadFiles,
    /// `unknown_tool`
    UnknownTool,
}

impl ToolName {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileSearch => "file_search",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
            Self::FunctionCall => "function_call",
            Self::SearchKnowledge => "search_knowledge",
            Self::LoadFiles => "load_files",
            Self::UnknownTool => "unknown_tool",
        }
    }
}

/// `start` or `done`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    /// The tool started.
    Start,
    /// The tool finished.
    Done,
}

impl ToolPhase {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Done => "done",
        }
    }
}

/// `text` or `reasoning`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    /// Assistant text.
    Text,
    /// `<think>` text of the vLLM adapter.
    Reasoning,
}

impl DeltaKind {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Reasoning => "reasoning",
        }
    }
}

/// `file` or `web`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    /// A chat attachment.
    File,
    /// A web page.
    Web,
}

impl CitationSource {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Web => "web",
        }
    }
}

/// One item of the `citations` event.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationItem {
    /// `file` or `web`.
    pub source: CitationSource,
    /// Title (web page title or attachment file name).
    pub title: String,
    /// Snippet (empty for file items).
    pub snippet: String,
    /// Web URL.
    pub url: Option<String>,
    /// Internal attachment id (file items).
    pub attachment_id: Option<Uuid>,
    /// Character span `(start, end)`.
    pub span: Option<(u64, u64)>,
}

/// `stream_started` payload.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStarted {
    /// The request id of the turn.
    pub request_id: Uuid,
    /// Pre-allocated assistant message id (the persisted one on replay).
    pub message_id: Uuid,
    /// `false` on replay.
    pub is_new_turn: bool,
    /// `token_estimate` of the kept thread summary, when one was kept.
    pub thread_summary_applied: Option<u32>,
}

/// `done` payload.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct DoneEvent {
    /// Provider usage `(input_tokens, output_tokens)`; zeros when unknown.
    pub usage: (u64, u64),
    /// Model that ran.
    pub effective_model: String,
    /// Model of the chat.
    pub selected_model: String,
    /// `allow` or `downgrade`.
    pub quota_decision: QuotaDecision,
    /// The selected model, when downgraded.
    pub downgrade_from: Option<String>,
    /// Why, when downgraded (never on replay).
    pub downgrade_reason: Option<DowngradeReason>,
    /// `None` omits the key (replay, or the post-commit read failed).
    pub quota_warnings: Option<Vec<QuotaWarningView>>,
}

/// `error` payload.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorEvent {
    /// Streaming error code.
    pub code: StreamErrorCode,
    /// Sanitized text.
    pub message: String,
}

/// An SSE event as the domain produces it.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// First event.
    StreamStarted(StreamStarted),
    /// Idle keep-alive before the first content event.
    Ping,
    /// A text fragment.
    Delta {
        /// `text` or `reasoning`.
        kind: DeltaKind,
        /// The fragment.
        content: String,
    },
    /// A tool event.
    Tool {
        /// `start` or `done`.
        phase: ToolPhase,
        /// Tool name.
        name: ToolName,
        /// Details object.
        details: Value,
    },
    /// Citations, at most once.
    Citations(Vec<CitationItem>),
    /// Terminal: the turn committed `completed`.
    Done(DoneEvent),
    /// Terminal: the stream failed.
    Error(ErrorEvent),
}

impl StreamEvent {
    /// The SSE event name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::StreamStarted(_) => "stream_started",
            Self::Ping => "ping",
            Self::Delta { .. } => "delta",
            Self::Tool { .. } => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// `done` and `error` end the stream.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }
}

/// A function call of a `tool_use` outcome.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    /// Provider call id.
    pub call_id: String,
    /// Function name.
    pub name: String,
    /// JSON arguments as text.
    pub arguments: String,
}

/// A provider annotation with the context needed to map it.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    /// The raw annotation object (`url_citation`, `file_citation`, ...).
    pub value: Value,
    /// Output item id, when known.
    pub item_id: Option<String>,
    /// Content part index, when known.
    pub content_index: Option<u32>,
    /// The full text of the output part that carries the annotation, set only
    /// for annotations taken from the `response.completed` payload (streamed
    /// annotations are resolved against the accumulated deltas).
    pub part_text: Option<String>,
}

/// The internal events every adapter translates its wire events into
/// (`cpt-cf-mini-chat-algo-streaming-turns-translate-provider-stream`).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// Assistant text.
    TextDelta {
        /// The fragment.
        text: String,
        /// Output item id.
        item_id: Option<String>,
        /// Content part index.
        content_index: Option<u32>,
    },
    /// Reasoning text (not persisted).
    ReasoningDelta {
        /// The fragment.
        text: String,
    },
    /// A tool started.
    ToolStart {
        /// Tool.
        name: ToolName,
        /// Details object.
        details: Value,
    },
    /// A tool finished.
    ToolDone {
        /// Tool.
        name: ToolName,
        /// Details object.
        details: Value,
    },
    /// An annotation to map into a citation.
    Annotation(Annotation),
    /// Normal or incomplete completion.
    Completed {
        /// Usage, when the provider reported one.
        usage: Option<UsageTokens>,
        /// Provider response id (kept internally).
        response_id: Option<String>,
        /// Incomplete reason; `None` for a normal completion.
        incomplete_reason: Option<String>,
    },
    /// The model asks for function calls.
    ToolUse {
        /// The calls.
        calls: Vec<FunctionCall>,
        /// Usage of this request.
        usage: Option<UsageTokens>,
    },
    /// The provider reported an error in the stream.
    ProviderError {
        /// Provider error code, when any.
        code: Option<String>,
        /// Raw provider message (sanitized by the pipeline, never sent as is).
        message: String,
        /// Usage reported with the failure.
        usage: Option<UsageTokens>,
    },
}

/// A failed provider call or stream, already classified
/// (`cpt-cf-mini-chat-algo-streaming-turns-classify-provider-error`).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderFailure {
    /// Streaming error code.
    pub code: StreamErrorCode,
    /// Client-safe message.
    pub message: String,
    /// `chat_turns.error_detail`, at most 1024 characters.
    pub detail: Option<String>,
    /// Usage reported with the failure.
    pub usage: Option<UsageTokens>,
}

impl ProviderFailure {
    /// A failure with the generic text of `code` and no detail.
    #[must_use]
    pub fn generic(code: StreamErrorCode) -> Self {
        Self {
            code,
            message: code.generic_message().to_owned(),
            detail: None,
            usage: None,
        }
    }
}
