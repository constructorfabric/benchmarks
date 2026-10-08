//! SSE relay of a live turn (DESIGN §3.3 "SSE Event Ordering", "`event: ping`",
//! close rules; ADR-0010).
//!
//! The relay yields `stream_started`, then the provider task's events, adding a
//! `ping` after every `sse_ping_interval_seconds` of idle time until the first
//! `delta` / `tool`. It ends right after the terminal event; when the provider
//! task ends without one (CAS lost, panic) it synthesizes
//! `error{stream_interrupted}`. Dropping the relay (client disconnect) cancels
//! the turn's token.

use std::time::Duration;

use futures::Stream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::rest::dto::PingData;
use crate::api::rest::sse::SseEvent;
use crate::domain::model::error_codes;

/// Client text of `stream_interrupted`.
const INTERRUPTED_MESSAGE: &str =
    "The response stream was interrupted; check the turn status for the final outcome";

/// Live stream of a turn: its first event, the provider task's channel and the
/// turn's cancellation token.
#[derive(Debug)]
pub struct LiveStream {
    pub events: mpsc::Receiver<SseEvent>,
    /// Cancelled when the client goes away.
    pub cancel: CancellationToken,
    /// `stream_started`.
    pub first: SseEvent,
}

/// Cancels the turn when the SSE body is dropped.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

struct Relay {
    first: Option<SseEvent>,
    rx: mpsc::Receiver<SseEvent>,
    ping: Duration,
    content_started: bool,
    finished: bool,
    _guard: CancelOnDrop,
}

impl Relay {
    async fn next(&mut self) -> Option<SseEvent> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        if self.finished {
            return None;
        }
        let received = if self.content_started {
            self.rx.recv().await
        } else {
            let Ok(ev) = tokio::time::timeout(self.ping, self.rx.recv()).await else {
                return Some(SseEvent::Ping(PingData {}));
            };
            ev
        };
        let Some(ev) = received else {
            // The provider task ended without a terminal event.
            self.finished = true;
            return Some(SseEvent::error(
                error_codes::STREAM_INTERRUPTED,
                INTERRUPTED_MESSAGE,
            ));
        };
        self.content_started |= ev.is_content();
        self.finished = ev.is_terminal();
        Some(ev)
    }
}

/// The SSE event sequence of a live turn.
pub fn live_events(live: LiveStream, ping: Duration) -> impl Stream<Item = SseEvent> + Send {
    let relay = Relay {
        first: Some(live.first),
        rx: live.events,
        ping,
        content_started: false,
        finished: false,
        _guard: CancelOnDrop(live.cancel),
    };
    futures::stream::unfold(relay, |mut relay| async move {
        let ev = relay.next().await?;
        Some((ev, relay))
    })
}
