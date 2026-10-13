//! SSE writer of the streaming endpoints (D "SSE Event Definitions", "SSE
//! Event Ordering", ADR-0010 ping rule).
//!
//! `stream_started` first, then the relayed events; `event: ping` /
//! `data: {}` after every `ping_interval` of idle time only until the first
//! `delta` / `tool`; afterwards axum's comment keep-alive (30 s). The stream
//! ends right after the terminal event. When the provider task ends without
//! a terminal event the writer sends `error{stream_interrupted}`. Dropping
//! the response body (client disconnect) cancels the turn.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;

use crate::api::rest::dto::{
    CitationsData, DeltaData, ErrorData, PingData, StreamStartedData, ToolData,
};
use crate::domain::services::replay::ReplayTurn;
use crate::domain::services::stream_service::{LiveTurn, StreamEvent, TurnStream};

/// Interval of the SSE comment keep-alive (after content started).
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Code synthesized when the provider task ended without a terminal event.
pub const STREAM_INTERRUPTED: &str = "stream_interrupted";

/// SSE response of a send / retry / edit.
#[must_use]
pub fn into_sse(stream: TurnStream, ping_interval: Duration) -> Response {
    match stream {
        TurnStream::Live(live) => sse_response(live_events(live, ping_interval)),
        TurnStream::Replay(replay) => sse_response(replay_events(replay)),
    }
}

fn sse_response<S>(events: S) -> Response
where
    S: Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response()
}

/// Writer state of a live turn.
struct Live {
    started: Option<StreamStartedData>,
    events: mpsc::Receiver<StreamEvent>,
    /// Cancels the turn when the response body is dropped.
    _cancel_on_drop: DropGuard,
    content_started: bool,
    finished: bool,
    ping_interval: Duration,
}

fn live_events(
    live: LiveTurn,
    ping_interval: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let state = Live {
        started: Some(live.started),
        events: live.events,
        _cancel_on_drop: live.cancel.drop_guard(),
        content_started: false,
        finished: false,
        ping_interval,
    };
    futures::stream::unfold(state, |mut st| async move {
        if let Some(started) = st.started.take() {
            return Some((Ok(json_event("stream_started", &started)), st));
        }
        if st.finished {
            return None;
        }
        let next = if st.content_started {
            st.events.recv().await
        } else {
            tokio::select! {
                ev = st.events.recv() => ev,
                () = tokio::time::sleep(st.ping_interval) => {
                    return Some((Ok(json_event("ping", &PingData {})), st));
                }
            }
        };
        let event = if let Some(ev) = next {
            st.content_started |= ev.is_content();
            st.finished = ev.is_terminal();
            to_sse(ev)
        } else {
            // The provider task ended without a terminal event.
            st.finished = true;
            json_event(
                "error",
                &ErrorData {
                    code: STREAM_INTERRUPTED.to_owned(),
                    message: "The response stream was interrupted".to_owned(),
                },
            )
        };
        Some((Ok(event), st))
    })
}

fn replay_events(r: ReplayTurn) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let events = vec![
        json_event("stream_started", &r.started),
        to_sse(StreamEvent::Delta {
            kind: crate::api::rest::dto::DeltaKind::Text,
            content: r.text,
        }),
        json_event("done", &r.done),
    ];
    futures::stream::iter(events.into_iter().map(Ok))
}

/// `event: <name>` + JSON `data` (`MiniChatSseEvent` wire format).
fn to_sse(ev: StreamEvent) -> Event {
    match ev {
        StreamEvent::Delta { kind, content } => json_event("delta", &DeltaData { kind, content }),
        StreamEvent::Tool {
            phase,
            name,
            details,
        } => json_event(
            "tool",
            &ToolData {
                phase,
                name,
                details,
            },
        ),
        StreamEvent::Citations(items) => json_event("citations", &CitationsData { items }),
        StreamEvent::Done(done) => json_event("done", &*done),
        StreamEvent::Error { code, message } => json_event("error", &ErrorData { code, message }),
    }
}

fn json_event<T: Serialize>(name: &'static str, data: &T) -> Event {
    Event::default()
        .event(name)
        .json_data(data)
        .unwrap_or_else(|_| Event::default().event(name).data("{}"))
}
