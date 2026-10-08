//! Streaming turns: send, replay, retry/edit streaming, provider task and
//! CAS-guarded finalization (DESIGN §3.3 streaming contract, §5.7).

pub mod finalize;
pub mod preflight;
pub mod replay;
pub mod send;
pub mod task;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;
use uuid::Uuid;

use crate::infra::llm::DeltaKind;

/// Client-facing citation.
#[derive(Debug, Clone, PartialEq)]
pub struct CitationOut {
    /// `file` | `web`
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(u64, u64)>,
}

/// One `quota_warnings` entry of `done`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarningOut {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    pub next_reset: Option<time::OffsetDateTime>,
}

/// `done` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct DoneData {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub downgraded: bool,
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<QuotaWarningOut>>,
}

/// Stable SSE events (DESIGN §3.3).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        thread_summary_applied: Option<i64>,
    },
    Ping,
    Delta {
        kind: DeltaKind,
        content: String,
    },
    Tool {
        phase: &'static str,
        name: String,
        details: Value,
    },
    Citations(Vec<CitationOut>),
    Done(Box<DoneData>),
    Error {
        code: String,
        message: String,
    },
}

impl StreamEvent {
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }
}

/// A live generation: `stream_started` plus the provider task channel.
pub struct LiveStream {
    pub started: StreamEvent,
    pub rx: mpsc::Receiver<StreamEvent>,
    /// Cancels the provider task when dropped (client disconnect).
    pub cancel_guard: DropGuard,
    pub ping_interval: std::time::Duration,
}

/// Result of stream setup.
pub enum StreamStart {
    Live(LiveStream),
    Replay(Vec<StreamEvent>),
}
