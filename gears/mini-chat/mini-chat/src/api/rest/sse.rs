//! SSE wire format of the stream events (DESIGN section 3.3, "SSE Event
//! Definitions"; platform note 01 section 2.11).

use std::convert::Infallible;
use std::time::Duration;

use axum::http::HeaderValue;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::api::rest::dto;
use crate::domain::services::stream::StreamStart;
use crate::domain::services::stream::events::{
    Citation, CitationSource, DeltaKind, DoneData, StreamEvent, StreamStartedData, ToolPhase,
};
use crate::domain::services::stream::relay::relay;

/// Interval of the SSE comment keep-alive (after content started no `ping`
/// events are sent, ADR-0010).
const KEEP_ALIVE: Duration = Duration::from_secs(30);

impl StreamEvent {
    /// The wire event: `event: <name>` and the JSON payload (`ping` is `{}`).
    pub fn into_sse_event(self) -> Event {
        let name = self.name();
        let data = match self {
            Self::StreamStarted(d) => json(&dto::StreamStartedData::from(d)),
            Self::Ping => json(&dto::PingData {}),
            Self::Delta { kind, content } => json(&dto::DeltaData {
                kind: match kind {
                    DeltaKind::Text => dto::DeltaKind::Text,
                    DeltaKind::Reasoning => dto::DeltaKind::Reasoning,
                },
                content,
            }),
            Self::Tool {
                phase,
                name,
                details,
            } => json(&dto::ToolData {
                phase: match phase {
                    ToolPhase::Start => dto::ToolPhase::Start,
                    ToolPhase::Done => dto::ToolPhase::Done,
                },
                name,
                details,
            }),
            Self::Citations(items) => json(&dto::CitationsData {
                items: items.into_iter().map(dto::Citation::from).collect(),
            }),
            Self::Done(d) => json(&dto::DoneData::from(d)),
            Self::Error { code, message } => json(&dto::ErrorData { code, message }),
        };
        Event::default().event(name).data(data)
    }
}

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|e| {
        tracing::error!(error = %e, "SSE payload serialization failed");
        "{}".to_owned()
    })
}

impl From<StreamStartedData> for dto::StreamStartedData {
    fn from(d: StreamStartedData) -> Self {
        Self {
            request_id: d.request_id,
            message_id: d.message_id,
            is_new_turn: d.is_new_turn,
            thread_summary_applied: d.thread_summary_applied.map(|s| dto::ThreadSummaryInfo {
                token_estimate: s.token_estimate,
            }),
        }
    }
}

impl From<Citation> for dto::Citation {
    fn from(c: Citation) -> Self {
        Self {
            source: match c.source {
                CitationSource::File => dto::CitationSource::File,
                CitationSource::Web => dto::CitationSource::Web,
            },
            title: c.title,
            url: c.url,
            attachment_id: c.attachment_id,
            snippet: c.snippet,
            span: c.span.map(|s| dto::TextSpan {
                start: s.start,
                end: s.end,
            }),
            score: None,
        }
    }
}

impl From<DoneData> for dto::DoneData {
    fn from(d: DoneData) -> Self {
        Self {
            usage: dto::Usage {
                input_tokens: d.usage.input_tokens,
                output_tokens: d.usage.output_tokens,
            },
            effective_model: d.effective_model,
            selected_model: d.selected_model,
            quota_decision: d.quota_decision.into(),
            downgrade_from: d.downgrade_from,
            downgrade_reason: d.downgrade_reason,
            quota_warnings: d
                .quota_warnings
                .map(|w| w.into_iter().map(dto::QuotaWarning::from).collect()),
        }
    }
}

/// The SSE response of a stream start: the live relay (dropping the body on
/// client disconnect cancels the provider task) or the buffered replay, with
/// a 30 s comment keep-alive.
#[must_use]
pub fn sse_response(start: StreamStart) -> Response {
    let events: BoxStream<'static, StreamEvent> = match start {
        StreamStart::Live(rx, guard) => relay(rx, guard).boxed(),
        StreamStart::Replay(events) => futures::stream::iter(events).boxed(),
    };
    let body = events.map(|ev| Ok::<_, Infallible>(ev.into_sse_event()));
    let mut resp = Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response();
    let headers = resp.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod sse_tests;
