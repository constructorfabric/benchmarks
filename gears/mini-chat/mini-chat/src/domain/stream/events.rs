//! Client SSE events produced by the stream service (wire names in [`StreamEvent::name`]).

use serde::Serialize;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummaryApplied {
    pub token_estimate: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStarted {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryApplied>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delta {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub phase: &'static str,
    pub name: String,
    pub details: Value,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub source: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<Span>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citations {
    pub items: Vec<Citation>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct UsageOut {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaWarningOut {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Done {
    pub usage: UsageOut,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarningOut>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorOut {
    pub code: String,
    pub message: String,
}

/// One client SSE event.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    Started(StreamStarted),
    Ping,
    Delta(Delta),
    Tool(Tool),
    Citations(Citations),
    Done(Done),
    Error(ErrorOut),
}

impl StreamEvent {
    /// SSE event name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Started(_) => "stream_started",
            Self::Ping => "ping",
            Self::Delta(_) => "delta",
            Self::Tool(_) => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// JSON payload.
    #[must_use]
    pub fn data(&self) -> String {
        let v = match self {
            Self::Started(s) => serde_json::to_string(s),
            Self::Ping => Ok("{}".to_owned()),
            Self::Delta(d) => serde_json::to_string(d),
            Self::Tool(t) => serde_json::to_string(t),
            Self::Citations(c) => serde_json::to_string(c),
            Self::Done(d) => serde_json::to_string(d),
            Self::Error(e) => serde_json::to_string(e),
        };
        v.unwrap_or_else(|_| "{}".to_owned())
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }

    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(self, Self::Delta(_) | Self::Tool(_))
    }

    #[must_use]
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error(ErrorOut {
            code: code.to_owned(),
            message: message.into(),
        })
    }
}
