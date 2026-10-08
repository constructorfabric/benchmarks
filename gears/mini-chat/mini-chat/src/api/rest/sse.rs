//! SSE relay: turns stream events into `text/event-stream` frames, sends
//! `ping` before the first content event, synthesizes `stream_interrupted`
//! when the provider task ends without a terminal event, and cancels the
//! turn when the response is dropped (client disconnect).

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;

use super::dto::{
    CitationsData, DeltaData, DeltaKind, DoneData, ErrorData, PingData, StreamStartedData,
    ThreadSummaryInfo, ToolData, ToolPhase,
};
use crate::domain::service::stream::{StreamEvent, StreamStart};

/// Comment keep-alive interval.
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);

fn json_event<T: serde::Serialize>(name: &str, data: &T) -> Event {
    Event::default()
        .event(name)
        .json_data(data)
        .unwrap_or_else(|_| Event::default().event(name).data("{}"))
}

/// Encode one stream event.
pub fn to_sse(ev: StreamEvent) -> Event {
    match ev {
        StreamEvent::Started {
            request_id,
            message_id,
            is_new_turn,
            thread_summary_applied,
        } => json_event(
            "stream_started",
            &StreamStartedData {
                request_id,
                message_id,
                is_new_turn,
                thread_summary_applied: thread_summary_applied.map(|t| ThreadSummaryInfo {
                    token_estimate: u32::try_from(t).unwrap_or_default(),
                }),
            },
        ),
        StreamEvent::Delta { reasoning, content } => json_event(
            "delta",
            &DeltaData {
                kind: if reasoning {
                    DeltaKind::Reasoning
                } else {
                    DeltaKind::Text
                },
                content,
            },
        ),
        StreamEvent::Tool {
            done,
            name,
            details,
        } => json_event(
            "tool",
            &ToolData {
                phase: if done {
                    ToolPhase::Done
                } else {
                    ToolPhase::Start
                },
                name,
                details,
            },
        ),
        StreamEvent::Citations(items) => json_event(
            "citations",
            &CitationsData {
                items: items.into_iter().map(Into::into).collect(),
            },
        ),
        StreamEvent::Done(d) => json_event("done", &DoneData::from(d)),
        StreamEvent::Error { code, message } => json_event("error", &ErrorData { code, message }),
    }
}

fn ping() -> Event {
    json_event("ping", &PingData {})
}

struct RelayState {
    rx: mpsc::Receiver<StreamEvent>,
    _guard: DropGuard,
    ping: Duration,
    content_started: bool,
    finished: bool,
}

/// Build the event stream of a stream start.
#[must_use]
pub fn event_stream(start: StreamStart, ping_interval: Duration) -> BoxStream<'static, Event> {
    match start {
        StreamStart::Replay(events) => stream::iter(events.into_iter().map(to_sse)).boxed(),
        StreamStart::Live(live) => {
            let state = RelayState {
                rx: live.events,
                _guard: live.cancel.drop_guard(),
                ping: ping_interval,
                content_started: false,
                finished: false,
            };
            stream::unfold(state, |mut st| async move {
                if st.finished {
                    return None;
                }
                let next = if st.content_started {
                    st.rx.recv().await
                } else {
                    match tokio::time::timeout(st.ping, st.rx.recv()).await {
                        Ok(ev) => ev,
                        Err(_) => return Some((ping(), st)),
                    }
                };
                if let Some(ev) = next {
                    if ev.is_content() {
                        st.content_started = true;
                    }
                    if ev.is_terminal() {
                        st.finished = true;
                    }
                    return Some((to_sse(ev), st));
                }
                st.finished = true;
                Some((
                    json_event(
                        "error",
                        &ErrorData {
                            code: "stream_interrupted".to_owned(),
                            message: "The stream ended unexpectedly".to_owned(),
                        },
                    ),
                    st,
                ))
            })
            .boxed()
        }
    }
}

/// SSE response of a stream start.
#[must_use]
pub fn sse_response(start: StreamStart, ping_interval: Duration) -> Response {
    let events = event_stream(start, ping_interval).map(Ok::<_, Infallible>);
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}
