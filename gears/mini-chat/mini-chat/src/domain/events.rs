//! Stream events relayed to the SSE writer.

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// `stream_started` payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamStarted {
    /// Turn request id.
    pub request_id: Uuid,
    /// Assistant message id (pre-allocated or persisted).
    pub message_id: Uuid,
    /// `false` on replay.
    pub is_new_turn: bool,
    /// Summary info when a thread summary is in the context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryApplied>,
}

/// `thread_summary_applied`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ThreadSummaryApplied {
    /// Summary token estimate.
    pub token_estimate: u32,
}

/// Citation item.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CitationItem {
    /// `file` or `web`.
    pub source: &'static str,
    /// Title.
    pub title: String,
    /// URL (web).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Attachment id (file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    /// Snippet.
    pub snippet: String,
    /// Span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

/// Character span.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Span {
    /// Start.
    pub start: usize,
    /// End.
    pub end: usize,
}

/// Token usage in `done`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct DoneUsage {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
}

/// `quota_warnings` entry.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuotaWarning {
    /// `premium` / `total`.
    pub tier: &'static str,
    /// `daily` / `monthly`.
    pub period: &'static str,
    /// Remaining percentage.
    pub remaining_percentage: u32,
    /// Warning flag.
    pub warning: bool,
    /// Exhausted flag.
    pub exhausted: bool,
    /// Next reset (only when warning or exhausted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset: Option<String>,
}

/// `done` payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Done {
    /// Usage.
    pub usage: DoneUsage,
    /// Effective model.
    pub effective_model: String,
    /// Selected model.
    pub selected_model: String,
    /// `allow` / `downgrade`.
    pub quota_decision: &'static str,
    /// Downgrade source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Downgrade reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    /// Quota warnings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// One relayed event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Text or reasoning delta.
    Delta { kind: &'static str, content: String },
    /// Tool lifecycle.
    Tool { phase: &'static str, name: String, details: Value },
    /// Citations.
    Citations(Vec<CitationItem>),
    /// Terminal success.
    Done(Done),
    /// Terminal error.
    Error { code: String, message: String },
}

impl StreamEvent {
    /// `true` for `done` / `error`.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// `true` for `delta` / `tool`.
    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta { .. } | Self::Tool { .. })
    }
}
