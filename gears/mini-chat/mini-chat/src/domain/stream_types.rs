//! SSE event payloads (stable public contract, DESIGN §3.3 "SSE Event Definitions").

use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// `thread_summary_applied` of `stream_started`.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ThreadSummaryInfo {
    pub token_estimate: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ToolData {
    pub phase: &'static str,
    pub name: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Citation {
    pub source: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
pub struct UsageData {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QuotaWarning {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DoneData {
    pub usage: UsageData,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// One SSE event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Started(StreamStartedData),
    Delta(DeltaData),
    Tool(ToolData),
    Citations(CitationsData),
    Done(DoneData),
    Error(ErrorData),
}

impl StreamEvent {
    /// SSE event name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Started(_) => "stream_started",
            Self::Delta(_) => "delta",
            Self::Tool(_) => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// JSON payload.
    #[must_use]
    pub fn data(&self) -> serde_json::Value {
        let v = match self {
            Self::Started(d) => serde_json::to_value(d),
            Self::Delta(d) => serde_json::to_value(d),
            Self::Tool(d) => serde_json::to_value(d),
            Self::Citations(d) => serde_json::to_value(d),
            Self::Done(d) => serde_json::to_value(d),
            Self::Error(d) => serde_json::to_value(d),
        };
        v.unwrap_or(serde_json::Value::Null)
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }

    /// Content events end the ping phase.
    #[must_use]
    pub const fn is_content(&self) -> bool {
        matches!(self, Self::Delta(_) | Self::Tool(_))
    }

    #[must_use]
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error(ErrorData { code: code.to_owned(), message: message.into() })
    }
}

/// Cancels the token when dropped (client disconnect / unsent response).
#[derive(Debug)]
pub struct CancelOnDrop(pub CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// A live generation: events from the provider task.
#[derive(Debug)]
pub struct LiveStream {
    pub rx: mpsc::Receiver<StreamEvent>,
    pub guard: CancelOnDrop,
    pub ping_interval_secs: u64,
}

/// What a send/retry/edit request produced.
#[derive(Debug)]
pub enum StreamStart {
    /// Idempotent replay of a completed turn.
    Replay(Vec<StreamEvent>),
    Live(LiveStream),
}
