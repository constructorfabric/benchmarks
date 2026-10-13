//! SSE wire format of the send endpoint
//! (`cpt-cf-mini-chat-algo-streaming-turns-relay-sse`): the domain events as
//! `event: <name>` / `data: <compact JSON>` frames and the 200 response.
//!
//! The ping timer, the ordering and the `stream_interrupted` synthesis live in
//! the domain relay (`domain::service::stream::relay`); this module only
//! formats and answers.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};

use super::dto::{
    Citation, CitationSourceDto, CitationsData, DeltaData, DeltaType, DoneData, ErrorData,
    PingData, QuotaDecisionDto, QuotaWarning, StreamStartedData, TextSpan, ThreadSummaryInfo,
    ToolData, ToolPhase, Usage,
};
use crate::domain::service::quota::preflight::QuotaDecision;
use crate::domain::service::stream::types::{
    CitationItem, CitationSource, DeltaKind, StreamEvent, ToolPhase as DomainToolPhase,
};

/// The `data` JSON of `event`, compact (keys of an absent optional value are
/// omitted, never `null`).
#[must_use]
pub fn event_data(event: &StreamEvent) -> String {
    let json = match event {
        StreamEvent::StreamStarted(started) => serde_json::to_string(&StreamStartedData {
            request_id: started.request_id,
            message_id: started.message_id,
            is_new_turn: started.is_new_turn,
            thread_summary_applied: started
                .thread_summary_applied
                .map(|token_estimate| ThreadSummaryInfo { token_estimate }),
        }),
        StreamEvent::Ping => serde_json::to_string(&PingData {}),
        StreamEvent::Delta { kind, content } => serde_json::to_string(&DeltaData {
            kind: match kind {
                DeltaKind::Text => DeltaType::Text,
                DeltaKind::Reasoning => DeltaType::Reasoning,
            },
            content: content.clone(),
        }),
        StreamEvent::Tool {
            phase,
            name,
            details,
        } => serde_json::to_string(&ToolData {
            phase: match phase {
                DomainToolPhase::Start => ToolPhase::Start,
                DomainToolPhase::Done => ToolPhase::Done,
            },
            name: name.as_str().to_owned(),
            details: details.clone(),
        }),
        StreamEvent::Citations(items) => serde_json::to_string(&CitationsData {
            items: items.iter().map(citation).collect(),
        }),
        StreamEvent::Done(done) => serde_json::to_string(&DoneData {
            usage: Usage {
                input_tokens: done.usage.0,
                output_tokens: done.usage.1,
            },
            effective_model: done.effective_model.clone(),
            selected_model: done.selected_model.clone(),
            quota_decision: match done.quota_decision {
                QuotaDecision::Allow => QuotaDecisionDto::Allow,
                QuotaDecision::Downgrade => QuotaDecisionDto::Downgrade,
            },
            downgrade_from: done.downgrade_from.clone(),
            downgrade_reason: done
                .downgrade_reason
                .map(|reason| reason.as_str().to_owned()),
            quota_warnings: done
                .quota_warnings
                .as_ref()
                .map(|warnings| warnings.iter().copied().map(QuotaWarning::from).collect()),
        }),
        StreamEvent::Error(error) => serde_json::to_string(&ErrorData {
            code: error.code.as_str().to_owned(),
            message: error.message.clone(),
        }),
    };
    json.unwrap_or_else(|_| "{}".to_owned())
}

fn citation(item: &CitationItem) -> Citation {
    Citation {
        source: match item.source {
            CitationSource::File => CitationSourceDto::File,
            CitationSource::Web => CitationSourceDto::Web,
        },
        title: item.title.clone(),
        snippet: item.snippet.clone(),
        url: item.url.clone(),
        attachment_id: item.attachment_id,
        span: item.span.map(|(start, end)| TextSpan { start, end }),
    }
}

/// One frame: `event: <name>\ndata: <json>\n\n`.
#[must_use]
pub fn format_frame(event: &StreamEvent) -> String {
    format!("event: {}\ndata: {}\n\n", event.name(), event_data(event))
}

/// The 200 `text/event-stream` response over `events`: `Cache-Control:
/// no-cache`, no `Connection: keep-alive`, no compression, every event flushed
/// as soon as it is yielded, an SSE comment keep-alive every 30 s. Dropping the
/// response body drops `events` (the client disconnect).
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-response-headers
pub fn sse_response<S>(events: S) -> Response
where
    S: Stream<Item = StreamEvent> + Send + 'static,
{
    let frames = events.map(|event| {
        Ok::<_, Infallible>(
            Event::default()
                .event(event.name())
                .data(event_data(&event)),
        )
    });
    Sse::new(frames)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response()
}
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-response-headers

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
