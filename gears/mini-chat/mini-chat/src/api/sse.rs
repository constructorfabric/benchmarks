//! SSE response adapter: relays domain events without buffering; cancels the turn on drop.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::domain::service::stream::{SseEvent, StreamStart};

fn to_event(e: &SseEvent) -> Event {
    Event::default().event(e.event).data(e.data.to_string())
}

/// Live stream: yields events from the provider task; dropping it cancels the turn.
struct LiveEvents {
    rx: mpsc::Receiver<SseEvent>,
    _guard: DropGuard,
}

impl Stream for LiveEvents {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(e)) => Poll::Ready(Some(Ok(to_event(&e)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Builds the SSE HTTP response for a stream start.
#[must_use]
pub fn sse_response(start: StreamStart) -> Response {
    let keep_alive = KeepAlive::new().interval(Duration::from_secs(30));
    match start {
        StreamStart::Replay(events) => {
            let items: Vec<Result<Event, Infallible>> =
                events.iter().map(|e| Ok(to_event(e))).collect();
            Sse::new(futures::stream::iter(items))
                .keep_alive(keep_alive)
                .into_response()
        }
        StreamStart::Live(live) => {
            let guard = live.cancel.drop_guard();
            Sse::new(LiveEvents {
                rx: live.rx,
                _guard: guard,
            })
            .keep_alive(keep_alive)
            .into_response()
        }
    }
}

/// Token type re-export for handlers.
pub type Cancel = CancellationToken;
