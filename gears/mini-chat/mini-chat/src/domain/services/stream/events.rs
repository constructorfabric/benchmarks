//! Stream events of the `messages:stream`, retry and edit responses (DESIGN
//! section 3.3, "SSE Event Definitions"). Domain values only: the REST layer
//! (`api::rest::sse`) turns them into wire events.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::services::quota_service::{QuotaDecisionKind, QuotaWarningView};

/// SSE `error` code: the provider task ended without a terminal event (CAS
/// lost, panic); synthesized by the relay.
pub const STREAM_INTERRUPTED: &str = "stream_interrupted";
/// SSE `error` code: the finalization transaction of a completed stream failed.
pub const FINALIZATION_FAILED: &str = "finalization_failed";
/// SSE `error` / turn error code: too many `web_search` calls in one turn.
pub const WEB_SEARCH_CALLS_EXCEEDED: &str = "web_search_calls_exceeded";
/// SSE `error` / turn error code: too many `code_interpreter` calls in one turn.
pub const CODE_INTERPRETER_CALLS_EXCEEDED: &str = "code_interpreter_calls_exceeded";
/// The knowledge-search loop exceeded `max_calls_per_message + 2` iterations.
pub const AGENTIC_ITERATIONS_EXCEEDED: &str = "agentic_iterations_exceeded";
/// The model requested a function tool the gear does not handle.
pub const UNEXPECTED_TOOL_USE: &str = "unexpected_tool_use";

/// Kind of a `delta` chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// Lifecycle phase of a `tool` event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolPhase {
    Start,
    Done,
}

/// `stream_started.thread_summary_applied`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadSummaryInfo {
    /// The stored `thread_summaries.token_estimate`.
    pub token_estimate: u32,
}

/// `stream_started` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    /// Pre-allocated (new turn) or persisted (replay) assistant message id.
    pub message_id: Uuid,
    pub is_new_turn: bool,
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CitationSource {
    File,
    Web,
}

/// Character range of a citation in the answer text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

/// One `citations` item (file citations already mapped to attachments).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<TextSpan>,
}

/// `done.usage`: token counts only. Live turns carry the provider usage of
/// the terminal event (zeros when the provider reported none); replays carry
/// the persisted assistant message counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageCounts {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// `done` payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoneData {
    pub usage: UsageCounts,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKind,
    /// Equals `selected_model`; present only for a downgrade.
    pub downgrade_from: Option<String>,
    /// Present only for a live downgrade (never on replay).
    pub downgrade_reason: Option<String>,
    /// CAS-winning completed turns only.
    pub quota_warnings: Option<Vec<QuotaWarningView>>,
}

/// One event of a turn's SSE stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    StreamStarted(StreamStartedData),
    Ping,
    Delta {
        kind: DeltaKind,
        content: String,
    },
    Tool {
        phase: ToolPhase,
        name: String,
        details: Value,
    },
    Citations(Vec<Citation>),
    Done(DoneData),
    Error {
        code: String,
        message: String,
    },
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
            Self::Error { .. } => "error",
        }
    }

    /// `done` and `error` end the stream.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// An `error` event.
    #[must_use]
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}
