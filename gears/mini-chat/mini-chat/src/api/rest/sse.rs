//! SSE relay of a streaming turn (DESIGN section 3.3 "SSE Event
//! Definitions", "`event: ping`", "SSE Event Ordering", "SSE stream close
//! rules"; ADR-0010).
//!
//! [`run_send`] runs the setup of a turn ([`StreamService::send`]) in a
//! separate spawned task and awaits it ([`run_setup`] does the same for the
//! retry / edit setup of the turn service): a client that disconnects before the
//! stream opens does not interrupt the setup. A live turn is wrapped in a
//! [`GuardedLive`] inside that task, so a turn whose response is never sent
//! is cancelled as soon as the result is dropped.
//!
//! [`relay`] turns the outcome into the SSE response:
//!
//! - a replay is sent as is (`stream_started`, `delta`, `done`);
//! - a live turn sends `stream_started`, then relays the event channel. A
//!   `ping` (`data: {}`) is sent after every `ping_interval` of idle time,
//!   only between `stream_started` and the first `delta` / `tool` event;
//! - the stream ends right after the terminal `done` / `error` event; when
//!   the channel closes without a terminal event, `error` with
//!   `stream_interrupted` is sent instead;
//! - dropping the response before the terminal event (client disconnect)
//!   cancels the turn token and drops the channel; the provider task then
//!   finalizes the turn as `cancelled` and nothing more is sent.
//!
//! The response carries `content-type: text/event-stream` and
//! `cache-control: no-cache` (set by axum), `x-accel-buffering: no` (no
//! proxy buffering) and an SSE comment keep-alive every
//! [`KEEP_ALIVE_INTERVAL`]. The gateway's compression layer never compresses
//! `text/event-stream`.

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use futures::stream::{self, StreamExt as _};
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;
use toolkit_security::SecurityContext;
use tracing::{error, warn};
use uuid::Uuid;

use super::dto::{MiniChatSseEvent, PingData};
use crate::domain::error::DomainError;
use crate::domain::stream::events::{StreamError, StreamStarted};
use crate::domain::stream::{
    LiveStream, SendOptions, SendRequest, StreamEvent, StreamService, StreamStart,
};

/// Interval of the axum SSE comment keep-alive (`:` line).
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// `x-accel-buffering: no`: proxies must not buffer the stream.
const X_ACCEL_BUFFERING: HeaderName = HeaderName::from_static("x-accel-buffering");

/// A live turn whose token is cancelled when it is dropped before
/// [`GuardedLive::disarm`].
#[derive(Debug)]
pub struct GuardedLive {
    first: StreamStarted,
    rx: mpsc::Receiver<StreamEvent>,
    guard: Option<DropGuard>,
}

impl GuardedLive {
    /// Takes ownership of `live`; dropping the result cancels `live.cancel`.
    #[must_use]
    pub fn new(live: LiveStream) -> Self {
        Self {
            first: live.first,
            rx: live.rx,
            guard: Some(live.cancel.drop_guard()),
        }
    }

    /// The turn ended (terminal event or closed channel): dropping no longer
    /// cancels it.
    fn disarm(&mut self) {
        if let Some(guard) = self.guard.take() {
            // The token is no longer needed once the turn ended.
            drop(guard.disarm());
        }
    }
}

/// Outcome of a successful send setup, with a live turn already guarded.
#[derive(Debug)]
pub enum Started {
    /// Idempotent replay events.
    Replay(Vec<StreamEvent>),
    /// A running turn.
    Live(GuardedLive),
}

impl From<StreamStart> for Started {
    fn from(start: StreamStart) -> Self {
        match start {
            StreamStart::Replay(events) => Self::Replay(events),
            StreamStart::Live(live) => Self::Live(GuardedLive::new(live)),
        }
    }
}

/// Runs [`StreamService::send`] in a spawned task and awaits its result.
///
/// # Errors
///
/// The [`DomainError`] of the setup; [`DomainError::Internal`] when the task
/// panicked.
pub async fn run_send(
    service: Arc<StreamService>,
    ctx: SecurityContext,
    chat_id: Uuid,
    request: SendRequest,
    options: SendOptions,
) -> Result<Started, DomainError> {
    run_setup(async move { service.send(ctx, chat_id, request, options).await }).await
}

/// Runs the setup `setup` of a streaming turn (send, retry, edit) in a
/// spawned task and awaits its result: a client disconnect does not
/// interrupt the setup, and a live turn whose response is never sent is
/// cancelled when the result is dropped.
///
/// # Errors
///
/// The [`DomainError`] of the setup; [`DomainError::Internal`] when the task
/// panicked.
pub async fn run_setup<F>(setup: F) -> Result<Started, DomainError>
where
    F: Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    let task = tokio::spawn(async move { setup.await.map(Started::from) });
    match task.await {
        Ok(result) => result,
        Err(err) => {
            error!(target: "mini_chat.sse", error = %err, "stream setup task failed");
            Err(DomainError::internal(format!(
                "stream setup task failed: {err}"
            )))
        }
    }
}

/// The SSE response of a started turn; pings every `ping_interval` of idle
/// time before the first content event.
#[must_use]
pub fn relay(started: Started, ping_interval: Duration) -> Response {
    let events = match started {
        Started::Replay(events) => {
            stream::iter(events.into_iter().map(MiniChatSseEvent::from)).boxed()
        }
        Started::Live(live) => live_events(live, ping_interval).boxed(),
    };
    sse_response(events)
}

/// The SSE response over `events`.
#[must_use]
pub fn sse_response<S>(events: S) -> Response
where
    S: Stream<Item = MiniChatSseEvent> + Send + 'static,
{
    let body =
        events.filter_map(|event| async move { to_sse_event(event).map(Ok::<_, Infallible>) });
    let mut response = Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response();
    response
        .headers_mut()
        .insert(X_ACCEL_BUFFERING, HeaderValue::from_static("no"));
    response
}

fn to_sse_event(event: MiniChatSseEvent) -> Option<Event> {
    match event.into_parts() {
        Ok((name, data)) => Some(Event::default().event(name).data(data)),
        Err(err) => {
            error!(target: "mini_chat.sse", error = %err, "SSE event could not be serialized");
            None
        }
    }
}

/// Relay state of a live turn.
struct LiveRelay {
    live: GuardedLive,
    first: Option<StreamStarted>,
    /// `Some` until the first `delta` / `tool` event.
    ping_interval: Option<Duration>,
    finished: bool,
}

impl LiveRelay {
    async fn next(&mut self) -> Option<MiniChatSseEvent> {
        if self.finished {
            return None;
        }
        if let Some(first) = self.first.take() {
            return Some(StreamEvent::StreamStarted(first).into());
        }
        let received = match self.ping_interval {
            Some(interval) => {
                tokio::select! {
                    event = self.live.rx.recv() => event,
                    () = tokio::time::sleep(interval) => {
                        return Some(MiniChatSseEvent::Ping(PingData {}));
                    }
                }
            }
            None => self.live.rx.recv().await,
        };
        let Some(event) = received else {
            self.finish();
            warn!(target: "mini_chat.sse", "turn event channel closed without a terminal event");
            return Some(StreamEvent::Error(StreamError::stream_interrupted()).into());
        };
        if matches!(event, StreamEvent::Delta(_) | StreamEvent::Tool(_)) {
            self.ping_interval = None;
        }
        if event.is_terminal() {
            self.finish();
        }
        Some(event.into())
    }

    fn finish(&mut self) {
        self.finished = true;
        self.live.disarm();
    }
}

fn live_events(
    live: GuardedLive,
    ping_interval: Duration,
) -> impl Stream<Item = MiniChatSseEvent> + Send + 'static {
    let first = live.first;
    let relay = LiveRelay {
        live,
        first: Some(first),
        ping_interval: (!ping_interval.is_zero()).then_some(ping_interval),
        finished: false,
    };
    stream::unfold(relay, |mut relay| async move {
        relay.next().await.map(|event| (event, relay))
    })
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
