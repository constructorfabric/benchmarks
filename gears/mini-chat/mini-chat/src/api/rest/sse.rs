//! SSE relay of a turn: `stream_started`, `ping` until the first content
//! event, the provider task's events, and exactly one terminal event. The
//! stream holds a drop guard: when the client disconnects, the turn's
//! cancellation token is cancelled.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;

use super::dto::sse_payload;
use crate::domain::stream::{StreamEvent, TurnStart};

fn event(ev: StreamEvent) -> Event {
    let (name, data) = sse_payload(ev);
    Event::default().event(name).data(data)
}

#[allow(clippy::unnecessary_wraps)] // the SSE stream item type is `Result<Event, Infallible>`
fn to_event(ev: StreamEvent) -> Result<Event, Infallible> {
    Ok(event(ev))
}

struct Relay {
    rx: mpsc::Receiver<StreamEvent>,
    _guard: DropGuard,
    content_started: bool,
    ping: Duration,
    finished: bool,
}

fn live_stream(
    started: StreamEvent,
    relay: Relay,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let first = stream::once(async move { to_event(started) });
    let rest = stream::unfold(relay, |mut r| async move {
        if r.finished {
            return None;
        }
        let next = if r.content_started {
            r.rx.recv().await.map(Some)
        } else {
            tokio::time::timeout(r.ping, r.rx.recv())
                .await
                .map_or(Some(None), |v| v.map(Some))
        };
        match next {
            // Ping (idle before the first content event).
            Some(None) => Some((to_event(StreamEvent::Ping), r)),
            Some(Some(ev)) => {
                if ev.is_content() {
                    r.content_started = true;
                }
                if ev.is_terminal() {
                    r.finished = true;
                }
                Some((to_event(ev), r))
            }
            // Channel closed without a terminal event.
            None => {
                r.finished = true;
                Some((
                    to_event(StreamEvent::error(
                        "stream_interrupted",
                        "The stream ended without a terminal event",
                    )),
                    r,
                ))
            }
        }
    });
    first.chain(rest)
}

/// Build the SSE response of a turn start.
pub fn sse_response(start: TurnStart) -> Response {
    let keep_alive = KeepAlive::new().interval(Duration::from_secs(30));
    match start {
        TurnStart::Replay(events) => {
            let s = stream::iter(events.into_iter().map(to_event));
            Sse::new(s).keep_alive(keep_alive).into_response()
        }
        TurnStart::Live(live) => {
            let relay = Relay {
                rx: live.rx,
                _guard: live.cancel.drop_guard(),
                content_started: false,
                ping: Duration::from_secs(live.ping_interval_secs.max(1)),
                finished: false,
            };
            Sse::new(live_stream(live.started, relay))
                .keep_alive(keep_alive)
                .into_response()
        }
    }
}
