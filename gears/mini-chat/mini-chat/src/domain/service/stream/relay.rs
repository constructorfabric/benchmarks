//! The relay between the provider task and the SSE response
//! (`cpt-cf-mini-chat-algo-streaming-turns-relay-sse`,
//! `cpt-cf-mini-chat-state-streaming-turns-sse-stream`): the bounded channel
//! receiver with its drop guard, the ping timer, the event ordering and the
//! `stream_interrupted` synthesis. The wire formatting is `api::rest::sse`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use tokio::sync::mpsc;
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;

use super::ports::{DisconnectStage, StreamMetrics};
use super::types::{ErrorEvent, StreamErrorCode, StreamEvent, StreamStarted};

/// Cancels the turn when the relay is dropped before a terminal event was
/// written (client disconnect, or a setup result that was never delivered)
/// and records `mini_chat_cancel_requested{trigger="disconnect"}` and
/// `mini_chat_stream_disconnected{stage}`. Disarmed once a terminal event
/// was written, so a normal completion records no disconnect.
pub struct DisconnectGuard {
    cancel: CancellationToken,
    metrics: Arc<dyn StreamMetrics>,
    armed: bool,
    first_delta_seen: bool,
}

impl DisconnectGuard {
    /// Creates an armed guard.
    #[must_use]
    pub fn new(cancel: CancellationToken, metrics: Arc<dyn StreamMetrics>) -> Self {
        Self {
            cancel,
            metrics,
            armed: true,
            first_delta_seen: false,
        }
    }

    /// Disarms the guard: dropping it changes nothing.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Notes that a `delta` was written (the stage of a later disconnect is
    /// then `mid_stream`).
    pub fn note_delta(&mut self) {
        self.first_delta_seen = true;
    }
}

// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-user-disconnects
// @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-on-drop
// @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-client-gone
impl Drop for DisconnectGuard {
    // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-drop-relay
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-cancel
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancel.cancel();
        self.metrics.cancel_requested();
        self.metrics.stream_disconnected(if self.first_delta_seen {
            DisconnectStage::MidStream
        } else {
            DisconnectStage::BeforeFirstToken
        });
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-record-metrics:p1:inst-metric-cancel
    // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-drop-relay
}
// @cpt-end:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-client-gone
// @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-on-drop
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-client-disconnect:p1:inst-user-disconnects

/// The receiver side of the turn's channel with its drop guard.
pub struct RelayHandle {
    guard: DisconnectGuard,
    rx: mpsc::Receiver<StreamEvent>,
    ping_interval: Duration,
}

impl RelayHandle {
    /// Wraps the receiver, the turn's cancellation token and the metrics sink.
    #[must_use]
    pub fn new(
        rx: mpsc::Receiver<StreamEvent>,
        cancel: CancellationToken,
        metrics: Arc<dyn StreamMetrics>,
        ping_interval: Duration,
    ) -> Self {
        Self {
            guard: DisconnectGuard::new(cancel, metrics),
            rx,
            ping_interval,
        }
    }

    /// Starts the relay: the stream yields `stream_started` first, then the
    /// channel events in arrival order with a `ping` after every
    /// `ping_interval` of idle time until the first `delta` or `tool`, closes
    /// right after the terminal event, and yields `error` `stream_interrupted`
    /// when the channel closes without one. Dropping the stream before the
    /// terminal event is a client disconnect.
    #[must_use]
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-return-relay
    pub fn into_stream(self, started: StreamStarted) -> RelayStream {
        RelayStream {
            handle: self,
            started: Some(started),
            timer: None,
            content_seen: false,
            closed: false,
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-return-relay
}

/// The event stream of a relay.
pub struct RelayStream {
    handle: RelayHandle,
    started: Option<StreamStarted>,
    timer: Option<Pin<Box<Sleep>>>,
    content_seen: bool,
    closed: bool,
}

impl RelayStream {
    fn close(&mut self) {
        self.closed = true;
        self.timer = None;
        self.handle.guard.disarm();
    }
}

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-sse-contract:p1
impl Stream for RelayStream {
    type Item = StreamEvent;

    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-while-relay
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        // @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-closed-final
        if this.closed {
            return Poll::Ready(None);
        }
        // @cpt-end:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-closed-final
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-started
        if let Some(started) = this.started.take() {
            this.timer = Some(Box::pin(tokio::time::sleep(this.handle.ping_interval)));
            return Poll::Ready(Some(StreamEvent::StreamStarted(started)));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-started
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-wait-event
        match this.handle.rx.poll_recv(cx) {
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-event
            Poll::Ready(Some(event)) => {
                match &event {
                    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-close-after-terminal
                    // @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-terminal-written
                    StreamEvent::Done(_) | StreamEvent::Error(_) => this.close(),
                    // @cpt-end:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-terminal-written
                    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-close-after-terminal
                    // @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-first-content
                    StreamEvent::Delta { .. } => {
                        this.handle.guard.note_delta();
                        this.content_seen = true;
                        this.timer = None;
                    }
                    // @cpt-end:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-first-content
                    StreamEvent::Tool { .. } => {
                        this.content_seen = true;
                        this.timer = None;
                    }
                    _ => {}
                }
                return Poll::Ready(Some(event));
            }
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-event
            // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-interrupted
            // @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-interrupted
            Poll::Ready(None) => {
                this.close();
                return Poll::Ready(Some(StreamEvent::Error(ErrorEvent {
                    code: StreamErrorCode::StreamInterrupted,
                    message: StreamErrorCode::StreamInterrupted
                        .generic_message()
                        .to_owned(),
                })));
            }
            // @cpt-end:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-interrupted
            // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-interrupted
            Poll::Pending => {}
        }
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-wait-event
        // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-ping
        // @cpt-begin:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-ping
        if !this.content_seen
            && let Some(timer) = this.timer.as_mut()
            && timer.as_mut().poll(cx).is_ready()
        {
            timer
                .as_mut()
                .reset(tokio::time::Instant::now() + this.handle.ping_interval);
            return Poll::Ready(Some(StreamEvent::Ping));
        }
        // @cpt-end:cpt-cf-mini-chat-state-streaming-turns-sse-stream:p1:inst-ping
        // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-write-ping
        Poll::Pending
    }
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-relay-sse:p1:inst-while-relay
}

#[cfg(test)]
#[path = "relay_tests.rs"]
mod tests;
