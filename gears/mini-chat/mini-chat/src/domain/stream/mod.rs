//! Streaming turns: send, replay, provider task and finalization.

pub mod finalize;
pub mod plan;
pub mod run;
pub mod send;

use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// A client-visible citation.
#[derive(Debug, Clone, PartialEq)]
pub struct CitationView {
    /// `file` | `web`.
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(usize, usize)>,
}

/// One `quota_warnings` entry of `done`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarningView {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<OffsetDateTime>,
}

/// Payload of the terminal `done` event.
#[derive(Debug, Clone, PartialEq)]
pub struct DoneView {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub downgrade: bool,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<QuotaWarningView>>,
}

/// Stable SSE events of a turn (DESIGN §3.3).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        thread_summary_tokens: Option<i64>,
    },
    Ping,
    Delta {
        kind: DeltaKind,
        content: String,
    },
    Tool {
        phase: &'static str,
        name: String,
        details: serde_json::Value,
    },
    Citations(Vec<CitationView>),
    Done(DoneView),
    Error {
        code: String,
        message: String,
    },
}

impl StreamEvent {
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    #[must_use]
    pub const fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }

    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

/// Outcome of the stream setup.
pub enum TurnStart {
    /// Replay of a completed turn (buffered events).
    Replay(Vec<StreamEvent>),
    /// A live generation.
    Live(LiveTurn),
}

/// A live generation: `stream_started` plus the provider task's channel.
pub struct LiveTurn {
    pub started: StreamEvent,
    pub rx: mpsc::Receiver<StreamEvent>,
    /// Cancelled when the client disconnects (the SSE stream is dropped).
    pub cancel: CancellationToken,
    pub ping_interval_secs: u64,
}

/// Request of `messages:stream`.
#[derive(Debug, Clone, Default)]
pub struct SendRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Vec<Uuid>,
    pub web_search: bool,
}
