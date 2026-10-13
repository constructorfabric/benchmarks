//! SSE writer: turns a `StreamStart` into an `text/event-stream` response.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;

use crate::domain::models::StreamEvent;
use crate::domain::services::stream::{LiveStream, StreamStart};

/// Comment keep-alive interval (hardcoded, DESIGN §SSE).
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);

/// Error code synthesized when the provider task ends without a terminal event.
pub const STREAM_INTERRUPTED: &str = "stream_interrupted";

/// Converts a domain event into an SSE event.
pub fn to_sse_event(ev: &StreamEvent) -> Event {
    let data = serde_json::to_string(&ev.data()).unwrap_or_else(|_| "{}".to_owned());
    Event::default().event(ev.name()).data(data)
}

fn interrupted() -> StreamEvent {
    StreamEvent::Error {
        code: STREAM_INTERRUPTED.to_owned(),
        message: "The stream ended unexpectedly".to_owned(),
    }
}

/// Live relay: forwards events until a terminal one; cancels the turn when
/// the client goes away (the body is dropped before the terminal event).
fn live_stream(live: LiveStream) -> impl Stream<Item = Result<Event, Infallible>> {
    let LiveStream { mut rx, cancel } = live;
    async_stream::stream! {
        let guard = cancel.drop_guard();
        let mut terminal = false;
        while let Some(ev) = rx.recv().await {
            let is_terminal = ev.is_terminal();
            yield Ok(to_sse_event(&ev));
            if is_terminal {
                terminal = true;
                break;
            }
        }
        // Completed normally (or the producer vanished): do not cancel.
        let _ = guard.disarm();
        if !terminal {
            yield Ok(to_sse_event(&interrupted()));
        }
    }
}

/// Builds the SSE response.
#[must_use]
pub fn sse_response(start: StreamStart) -> Response {
    match start {
        StreamStart::Replay(events) => {
            let s = futures::stream::iter(events.into_iter().map(|e| Ok::<_, Infallible>(to_sse_event(&e))));
            Sse::new(s)
                .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
                .into_response()
        }
        StreamStart::Live(live) => Sse::new(live_stream(live))
            .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
            .into_response(),
    }
}
