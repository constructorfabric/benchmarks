//! SSE event payloads of the public streaming contract (DESIGN §3.3 "SSE Event Definitions").

use uuid::Uuid;

use crate::domain::quota::QuotaWarning;

/// `stream_started.thread_summary_applied`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// `event: stream_started`
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// `event: delta`
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    pub content: String,
}

/// Tool lifecycle phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ToolPhase {
    Start,
    Done,
}

/// `event: tool`
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

/// Citation source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum CitationSource {
    File,
    Web,
}

/// Character span within the response text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// One citation.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// `event: citations`
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// Token usage counters of `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// `event: done`
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    /// `allow` | `downgrade`
    pub quota_decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<Object>>)]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// `event: error`
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// One SSE event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted(StreamStartedData),
    Ping,
    Delta(DeltaData),
    Tool(ToolData),
    Citations(CitationsData),
    Done(DoneData),
    Error(ErrorData),
}

impl StreamEvent {
    /// SSE `event:` name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::StreamStarted(_) => "stream_started",
            Self::Ping => "ping",
            Self::Delta(_) => "delta",
            Self::Tool(_) => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// SSE `data:` JSON payload.
    #[must_use]
    pub fn data(&self) -> String {
        let v = match self {
            Self::StreamStarted(d) => serde_json::to_string(d),
            Self::Ping => Ok("{}".to_owned()),
            Self::Delta(d) => serde_json::to_string(d),
            Self::Tool(d) => serde_json::to_string(d),
            Self::Citations(d) => serde_json::to_string(d),
            Self::Done(d) => serde_json::to_string(d),
            Self::Error(d) => serde_json::to_string(d),
        };
        v.unwrap_or_else(|_| "{}".to_owned())
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }

    #[must_use]
    pub fn error(code: &str, message: impl Into<String>) -> Self {
        Self::Error(ErrorData { code: code.to_owned(), message: message.into() })
    }
}
