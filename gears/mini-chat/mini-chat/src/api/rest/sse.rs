//! SSE relay: bounded channel → `text/event-stream` (no buffering).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::domain::events::{StreamEvent, StreamStarted};
use crate::domain::finalization::TurnCtx;
use crate::domain::service::Svc;
use crate::domain::stream_service::{ReplayData, StreamStart};

/// Keep-alive comment interval after content starts.
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);

fn event(name: &str, data: &impl Serialize) -> Event {
    Event::default().event(name).json_data(data).unwrap_or_else(|_| Event::default().event(name).data("{}"))
}

/// Converts a stream event to an SSE event.
pub fn to_sse(ev: &StreamEvent) -> Event {
    match ev {
        StreamEvent::Delta { kind, content } => event("delta", &json!({"type": kind, "content": content})),
        StreamEvent::Tool { phase, name, details } => {
            event("tool", &json!({"phase": phase, "name": name, "details": details}))
        }
        StreamEvent::Citations(items) => event("citations", &json!({"items": items})),
        StreamEvent::Done(d) => event("done", d),
        StreamEvent::Error { code, message } => event("error", &json!({"code": code, "message": message})),
    }
}

fn sse_response<S>(stream: S) -> Response
where
    S: futures::Stream<Item = Result<Event, Infallible>> + Send + 'static,
{
    Sse::new(stream).keep_alive(KeepAlive::new().interval(KEEP_ALIVE)).into_response()
}

/// Replay stream: `stream_started`, one `delta`, `done`.
#[must_use]
pub fn replay_response(r: ReplayData) -> Response {
    let events = vec![
        Ok(event("stream_started", &r.started)),
        Ok(to_sse(&StreamEvent::Delta { kind: "text", content: r.text })),
        Ok(to_sse(&StreamEvent::Done(r.done))),
    ];
    sse_response(futures::stream::iter(events))
}

/// Live stream: spawns the provider task and relays its events.
#[must_use]
pub fn live_response(svc: Arc<Svc>, turn: Box<TurnCtx>) -> Response {
    let cap = usize::from(svc.cfg.streaming.sse_channel_capacity);
    let ping = Duration::from_secs(u64::from(svc.cfg.streaming.sse_ping_interval_seconds));
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(cap);
    let cancel = CancellationToken::new();
    let started: StreamStarted = Svc::stream_started(&turn);
    tokio::spawn(svc.run_turn(turn, tx, cancel.clone()));
    let stream = async_stream::stream! {
        let _guard = cancel.drop_guard();
        yield Ok::<Event, Infallible>(event("stream_started", &started));
        let mut content_started = false;
        loop {
            let next = if content_started {
                Some(rx.recv().await)
            } else {
                tokio::select! {
                    ev = rx.recv() => Some(ev),
                    () = tokio::time::sleep(ping) => None,
                }
            };
            let Some(next) = next else {
                yield Ok(event("ping", &json!({})));
                continue;
            };
            if let Some(ev) = next {
                if ev.is_content() {
                    content_started = true;
                }
                let terminal = ev.is_terminal();
                yield Ok(to_sse(&ev));
                if terminal {
                    break;
                }
            } else {
                yield Ok(to_sse(&StreamEvent::Error {
                    code: "stream_interrupted".into(),
                    message: "The stream was interrupted".into(),
                }));
                break;
            }
        }
    };
    sse_response(stream)
}

/// Response for a stream setup result.
#[must_use]
pub fn stream_response(svc: Arc<Svc>, start: StreamStart) -> Response {
    match start {
        StreamStart::Replay(r) => replay_response(*r),
        StreamStart::Live(turn) => live_response(svc, turn),
    }
}
