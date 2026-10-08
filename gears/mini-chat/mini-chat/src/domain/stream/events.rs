//! SSE events of the send, retry and edit streams (DESIGN "SSE Event Definitions") and their
//! JSON payloads. The HTTP layer writes each event as `event: <name>` + `data: <json>`
//! (`crate::api::sse`); `crate::api::dto::stream` documents the same wire format.

use std::pin::Pin;

use futures::Stream;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::domain::quota::QuotaWarning;

/// The events of one stream, in emission order; the stream ends after a terminal event.
pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// Lifecycle phase of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPhase {
    Start,
    Done,
}

/// `source` of a citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CitationSource {
    File,
    Web,
}

/// Character range of a citation in the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// One item of the `citations` event; optional fields are omitted when `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CitationDto {
    pub source: CitationSource,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
}

/// Token counts of the `done` event.
#[allow(clippy::struct_field_names)] // wire names
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct UsageDto {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Payload of the `done` event; optional fields are omitted when `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DonePayload {
    pub usage: UsageDto,
    pub effective_model: String,
    pub selected_model: String,
    /// `allow` or `downgrade`.
    pub quota_decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// One SSE event.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// First event of every stream.
    StreamStarted {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        /// Token estimate of the thread summary in the context, when one was included.
        thread_summary_applied: Option<i64>,
    },
    /// Keepalive before the first content event.
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
    Citations(Vec<CitationDto>),
    /// Terminal: the turn was committed as `completed`.
    Done(DonePayload),
    /// Terminal: `{code, message}` (message sanitized).
    Error {
        code: String,
        message: String,
    },
}

impl StreamEvent {
    /// The SSE event name.
    #[must_use]
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::StreamStarted { .. } => "stream_started",
            Self::Ping => "ping",
            Self::Delta { .. } => "delta",
            Self::Tool { .. } => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error { .. } => "error",
        }
    }

    /// Whether the stream ends after this event (`done` or `error`).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    /// The SSE `data` payload.
    #[must_use]
    pub fn data(&self) -> Value {
        match self {
            Self::StreamStarted {
                request_id,
                message_id,
                is_new_turn,
                thread_summary_applied,
            } => {
                let mut data = json!({
                    "request_id": request_id,
                    "message_id": message_id,
                    "is_new_turn": is_new_turn,
                });
                if let Some(token_estimate) = thread_summary_applied {
                    data["thread_summary_applied"] = json!({ "token_estimate": token_estimate });
                }
                data
            }
            Self::Ping => json!({}),
            Self::Delta { kind, content } => json!({ "type": kind, "content": content }),
            Self::Tool {
                phase,
                name,
                details,
            } => json!({ "phase": phase, "name": name, "details": details }),
            Self::Citations(items) => json!({ "items": items }),
            Self::Done(done) => serde_json::to_value(done).unwrap_or_else(|err| {
                tracing::error!(error = %err, "failed to serialize the done event");
                json!({})
            }),
            Self::Error { code, message } => json!({ "code": code, "message": message }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_match_the_wire_contract() {
        let id = Uuid::from_u128(1);
        let started = StreamEvent::StreamStarted {
            request_id: id,
            message_id: id,
            is_new_turn: true,
            thread_summary_applied: Some(42),
        };
        assert_eq!(started.event_name(), "stream_started");
        assert_eq!(
            started.data(),
            json!({"request_id": id, "message_id": id, "is_new_turn": true,
                   "thread_summary_applied": {"token_estimate": 42}})
        );
        assert_eq!(StreamEvent::Ping.data(), json!({}));
        let citation = CitationDto {
            source: CitationSource::Web,
            title: "t".into(),
            url: Some("https://e.x".into()),
            attachment_id: None,
            snippet: "s".into(),
            span: Some(TextSpan { start: 1, end: 2 }),
        };
        assert_eq!(
            StreamEvent::Citations(vec![citation]).data(),
            json!({"items": [{"source": "web", "title": "t", "url": "https://e.x",
                              "snippet": "s", "span": {"start": 1, "end": 2}}]})
        );
        let tool = StreamEvent::Tool {
            phase: ToolPhase::Done,
            name: "file_search".into(),
            details: json!({"files_searched": 0}),
        };
        assert_eq!(
            tool.data(),
            json!({"phase": "done", "name": "file_search", "details": {"files_searched": 0}})
        );
        assert!(!tool.is_terminal());
        let error = StreamEvent::Error {
            code: "provider_error".into(),
            message: "m".into(),
        };
        assert!(error.is_terminal());
        assert_eq!(error.event_name(), "error");
    }
}
