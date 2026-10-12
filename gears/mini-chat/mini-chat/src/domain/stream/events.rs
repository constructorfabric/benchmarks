//! Domain stream events (DESIGN section 3.3 "SSE Event Definitions", "SSE
//! Event Ordering", "Streaming error codes").
//!
//! The stream service produces these events; the SSE layer serializes them
//! (`event: <name>`, `data: <json>`) and adds `ping` keepalives itself. A
//! well-formed stream is `stream_started (delta | tool)* citations? (done |
//! error)`; [`StreamEvent::is_terminal`] marks the last event. Payloads never
//! carry provider identifiers.

use uuid::Uuid;

use crate::domain::quota_service::{DowngradeReason, QuotaDecisionKind, QuotaWarning};

// ── Streaming error codes ────────────────────────────────────────────────

/// Provider error, invalid response or provider stream failure.
pub const PROVIDER_ERROR: &str = "provider_error";
/// Provider (or gateway) timeout.
pub const PROVIDER_TIMEOUT: &str = "provider_timeout";
/// Provider 429.
pub const RATE_LIMITED: &str = "rate_limited";
/// More `web_search` calls started than `quota.web_search_max_calls_per_message`.
pub const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
/// More `code_interpreter` calls started than
/// `quota.code_interpreter_max_calls_per_message`.
pub const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
/// The knowledge-search agentic loop exceeded
/// `knowledge_search.max_calls_per_message + 2` provider iterations.
pub const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
/// The model requested a function tool the gear does not handle (any tool use
/// while knowledge search is off).
pub const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";
/// The assistant message could not be persisted; the turn was finalized as
/// `failed` instead of `completed`.
pub const MESSAGE_PERSISTENCE_FAILED: &str = "message_persistence_failed";
/// The finalization transaction of a completed stream failed; the turn stays
/// `running` until the orphan watchdog finalizes it.
pub const FINALIZATION_FAILED: &str = "finalization_failed";
/// The provider task ended without a terminal event (synthesized by the SSE
/// relay when the event channel closes without `done` / `error`).
pub const STREAM_INTERRUPTED: &str = "stream_interrupted";

/// Client message of [`WEB_SEARCH_CALLS_EXCEEDED`].
pub const WEB_SEARCH_CALLS_EXCEEDED_MESSAGE: &str =
    "The model exceeded the web search call limit for one message";
/// Client message of [`CODE_INTERPRETER_CALLS_EXCEEDED`].
pub const CODE_INTERPRETER_CALLS_EXCEEDED_MESSAGE: &str =
    "The model exceeded the code interpreter call limit for one message";
/// Client message of [`AGENTIC_ITERATIONS_EXCEEDED`].
pub const AGENTIC_ITERATIONS_EXCEEDED_MESSAGE: &str =
    "The model exceeded the knowledge search iteration limit for one message";
/// Client message of [`UNEXPECTED_TOOL_USE`].
pub const UNEXPECTED_TOOL_USE_MESSAGE: &str = "The model requested a tool that is not available";
/// Client message of [`MESSAGE_PERSISTENCE_FAILED`].
pub const MESSAGE_PERSISTENCE_FAILED_MESSAGE: &str = "The answer could not be saved";
/// Client message of [`FINALIZATION_FAILED`].
pub const FINALIZATION_FAILED_MESSAGE: &str = "The turn could not be finalized";
/// Client message of [`STREAM_INTERRUPTED`].
pub const STREAM_INTERRUPTED_MESSAGE: &str = "The stream ended before the turn finished";

// ── Payloads ─────────────────────────────────────────────────────────────

/// `event: stream_started`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamStarted {
    /// Resolved turn correlation key.
    pub request_id: Uuid,
    /// Pre-allocated (new turn) or persisted (replay) assistant message id.
    pub message_id: Uuid,
    /// `true` for a new generation, `false` for an idempotent replay.
    pub is_new_turn: bool,
    /// `token_estimate` of the thread summary when it was included in the
    /// context (serialized as `{"token_estimate": n}`).
    pub thread_summary_applied: Option<u32>,
}

/// `delta.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeltaKind {
    Text,
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

/// `event: delta`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub kind: DeltaKind,
    pub content: String,
}

/// `tool.phase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolPhase {
    Start,
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

/// `event: tool`.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolActivity {
    pub phase: ToolPhase,
    /// `file_search`, `web_search`, `code_interpreter`, ...
    pub name: String,
    /// Non-sensitive tool metadata (an object).
    pub details: serde_json::Value,
}

/// `citations.items[].source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CitationSource {
    File,
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

/// `citations.items[].span`: character offsets into the answer part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// One citation. File citations carry the internal `attachment_id` and the
/// attachment filename as `title`; web citations carry `url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<TextSpan>,
}

/// `done.usage` (token counts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DoneUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// `event: done`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub usage: DoneUsage,
    pub effective_model: String,
    /// `chats.model`.
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKind,
    /// `Some(selected_model)` on a downgrade.
    pub downgrade_from: Option<String>,
    /// Omitted on replay.
    pub downgrade_reason: Option<DowngradeReason>,
    /// Present only on CAS-winning completed turns.
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// `event: error`: a stable streaming error code and a sanitized message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    pub code: String,
    pub message: String,
}

impl StreamError {
    /// Error event with `code` and `message`.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The `stream_interrupted` event the relay sends when the event channel
    /// closes without a terminal event while the client is still connected.
    #[must_use]
    pub fn stream_interrupted() -> Self {
        Self::new(STREAM_INTERRUPTED, STREAM_INTERRUPTED_MESSAGE)
    }
}

// ── Event ────────────────────────────────────────────────────────────────

/// One domain stream event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted(StreamStarted),
    Delta(Delta),
    Tool(ToolActivity),
    Citations(Vec<Citation>),
    Done(Box<Done>),
    Error(StreamError),
}

impl StreamEvent {
    /// SSE event name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::StreamStarted(_) => "stream_started",
            Self::Delta(_) => "delta",
            Self::Tool(_) => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// Whether the event ends the stream (`done` / `error`).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }

    /// A text delta.
    #[must_use]
    pub fn text(content: impl Into<String>) -> Self {
        Self::Delta(Delta {
            kind: DeltaKind::Text,
            content: content.into(),
        })
    }

    /// An error event.
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error(StreamError::new(code, message))
    }
}

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
