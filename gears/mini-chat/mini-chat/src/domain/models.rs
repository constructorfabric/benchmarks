//! Domain value types: turn states, SSE stream events and their payloads.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// Turn state (`chat_turns.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TurnState {
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Running,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Turn Status API name (`running|done|error|cancelled`).
    #[must_use]
    pub const fn api_state(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "done",
            Self::Failed => "error",
            Self::Cancelled => "cancelled",
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// Attachment kinds.
pub mod attachment_kind {
    pub const DOCUMENT: &str = "document";
    pub const IMAGE: &str = "image";
}

/// `thread_summary_applied` in `stream_started`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ThreadSummaryInfo {
    pub token_estimate: u32,
}

/// `stream_started` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Character span of a citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

/// Citation item of the `citations` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
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

/// Token usage of the `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DoneUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Quota warning entry (`done.quota_warnings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QuotaWarning {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset: Option<DateTime<Utc>>,
}

/// `done` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoneData {
    pub usage: DoneUsage,
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

/// One SSE event of the stream contract.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted(StreamStartedData),
    Ping,
    Delta { kind: &'static str, content: String },
    Tool { phase: &'static str, name: String, details: Value },
    Citations(Vec<Citation>),
    Done(Box<DoneData>),
    Error { code: String, message: String },
}

impl StreamEvent {
    /// SSE `event:` name.
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

    /// SSE `data:` JSON payload.
    #[must_use]
    pub fn data(&self) -> Value {
        match self {
            Self::StreamStarted(d) => serde_json::to_value(d).unwrap_or(Value::Null),
            Self::Ping => serde_json::json!({}),
            Self::Delta { kind, content } => serde_json::json!({"type": kind, "content": content}),
            Self::Tool {
                phase,
                name,
                details,
            } => serde_json::json!({"phase": phase, "name": name, "details": details}),
            Self::Citations(items) => serde_json::json!({"items": items}),
            Self::Done(d) => serde_json::to_value(d).unwrap_or(Value::Null),
            Self::Error { code, message } => serde_json::json!({"code": code, "message": message}),
        }
    }

    /// Whether the event ends the stream.
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }
}
