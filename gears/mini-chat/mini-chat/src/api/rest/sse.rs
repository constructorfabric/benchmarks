//! SSE adaptation of [`StreamEvent`]s. Events are relayed one by one (no
//! buffering); dropping the response cancels the turn.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;

use super::dto::{
    Citation, CitationsData, DeltaData, DoneData, ErrorData, StreamStartedData, ThreadSummaryInfo, ToolData,
};
use crate::domain::service::stream::{StreamEvent, StreamStart};
use crate::infra::llm::codes;

fn json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{}".to_owned())
}

/// Converts a stream event to its SSE wire form.
#[must_use]
pub fn to_sse(ev: StreamEvent) -> Event {
    let (name, data) = match ev {
        StreamEvent::Started {
            request_id,
            message_id,
            is_new_turn,
            thread_summary_token_estimate,
        } => (
            "stream_started",
            json(&StreamStartedData {
                request_id,
                message_id,
                is_new_turn,
                thread_summary_applied: thread_summary_token_estimate
                    .map(|token_estimate| ThreadSummaryInfo { token_estimate }),
            }),
        ),
        StreamEvent::Ping => ("ping", "{}".to_owned()),
        StreamEvent::Delta { kind, content } => ("delta", json(&DeltaData { kind, content })),
        StreamEvent::Tool { phase, name, details } => ("tool", json(&ToolData { phase, name, details })),
        StreamEvent::Citations(items) => (
            "citations",
            json(&CitationsData {
                items: items.into_iter().map(Citation::from).collect(),
            }),
        ),
        StreamEvent::Done(d) => ("done", json(&DoneData::from(d))),
        StreamEvent::Error { code, message } => ("error", json(&ErrorData { code, message })),
    };
    Event::default().event(name).data(data)
}

fn into_stream(start: StreamStart) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    async_stream::stream! {
        match start {
            StreamStart::Replay(events) => {
                for ev in events {
                    yield Ok(to_sse(ev));
                }
            }
            StreamStart::Live { mut events, cancel } => {
                let _guard = cancel.drop_guard();
                let mut terminal = false;
                while let Some(ev) = events.recv().await {
                    terminal = ev.is_terminal();
                    yield Ok(to_sse(ev));
                    if terminal {
                        break;
                    }
                }
                if !terminal {
                    yield Ok(to_sse(StreamEvent::Error {
                        code: codes::STREAM_INTERRUPTED.to_owned(),
                        message: "The stream was interrupted".to_owned(),
                    }));
                }
            }
        }
    }
}

/// SSE response of a stream setup result.
pub fn sse_response(start: StreamStart) -> Response {
    let mut resp = Sse::new(into_stream(start))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
        .into_response();
    resp.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );
    resp
}
