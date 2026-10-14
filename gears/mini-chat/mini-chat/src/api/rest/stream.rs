//! The SSE responses of `POST /v1/chats/{id}/messages:stream` (slices S1-setup-path and
//! S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`): the replay stream, the
//! silent response of a dropped connection and the SSE relay of the provider task.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use futures::stream;
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;

use super::dto::{CitationsData, DeltaData, DoneData, StreamStartedData, ToolData};
use super::error::sse_error_data;
use crate::domain::error::StreamErrorCode;
use crate::domain::streaming::provider_errors::generic_message;
use crate::domain::streaming::{RelayEvent, StartedTurn, StreamEvent, StreamStarted};

/// The hardcoded interval of the SSE comment keep-alive of the HTTP server (30 s).
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// The wire form of a domain event: `event: <name>` and `data: <JSON payload>`.
fn wire_event(event: StreamEvent) -> Event {
    let (name, data) = match event {
        StreamEvent::StreamStarted(started) => (
            "stream_started",
            serde_json::to_value(StreamStartedData::from(started)),
        ),
        StreamEvent::Delta(delta) => ("delta", serde_json::to_value(DeltaData::from(delta))),
        StreamEvent::Done(done) => ("done", serde_json::to_value(DoneData::from(done))),
    };
    let data = data.unwrap_or_else(|error| {
        tracing::error!(event = name, %error, "an SSE payload could not be serialized");
        serde_json::json!({})
    });
    Event::default().event(name).data(data.to_string())
}

/// The `event: error` of a terminal failure.
fn error_event(code: StreamErrorCode, message: &str) -> Event {
    Event::default()
        .event("error")
        .data(sse_error_data(code, message).to_string())
}

/// The SSE response of `events`: HTTP 200, `Content-Type: text/event-stream`,
/// `Cache-Control: no-cache`, no `Connection: keep-alive` header, the comment keep-alive of the
/// server. `guard` is held by the body: dropping the response (the client went away) cancels the
/// turn's cancellation token.
fn sse_response(events: Vec<Event>, guard: DropGuard) -> Response {
    let body = stream::iter(events).map(move |event| {
        // the guard lives as long as the body; dropping the body cancels the token
        let _ = &guard;
        Ok::<Event, Infallible>(event)
    });
    Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response()
}

/// The replay stream: the three events of the replay algorithm, then the end of the stream.
#[must_use]
pub fn replay_response(events: Vec<StreamEvent>, guard: DropGuard) -> Response {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-open
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-return
    // the SSE response, closed by the server right after `done`
    sse_response(events.into_iter().map(wire_event).collect(), guard)
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-replay:p1:inst-replay-open
}

/// The response of a turn whose connection was dropped after the reserve committed: no SSE
/// event is written.
#[must_use]
pub fn silent_response(guard: DropGuard) -> Response {
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-dropped
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-return-before-provider
    sse_response(Vec::new(), guard)
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-return-before-provider
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-dropped
}

/// The wire form of a provider-driven event.
fn relay_wire(event: RelayEvent) -> Event {
    let (name, data) = match event {
        RelayEvent::Delta(delta) => ("delta", serde_json::to_value(DeltaData::from(delta))),
        RelayEvent::Tool(tool) => ("tool", serde_json::to_value(ToolData::from(tool))),
        RelayEvent::Citations(citations) => (
            "citations",
            serde_json::to_value(CitationsData::from(citations)),
        ),
        RelayEvent::Done(done) => ("done", serde_json::to_value(DoneData::from(done))),
        RelayEvent::Error(error) => {
            return error_event(error.code, &error.message);
        }
    };
    let data = data.unwrap_or_else(|error| {
        tracing::error!(event = name, %error, "an SSE payload could not be serialized");
        serde_json::json!({})
    });
    Event::default().event(name).data(data.to_string())
}

/// The `event: ping` with an empty JSON object as data.
fn ping_event() -> Event {
    Event::default().event("ping").data("{}")
}

/// Where the relay is in the stream state machine.
struct Relay {
    /// `stream_started`, until it was emitted.
    first: Option<Event>,
    events: mpsc::Receiver<RelayEvent>,
    ping_interval: Duration,
    /// A `delta` or `tool` event was emitted: no `ping` follows.
    content_seen: bool,
    /// The terminal event was emitted: nothing follows it.
    finished: bool,
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-client-dropped
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-cancel
    // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-client-gone
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-client-closes
    /// Dropping the relay (the client went away) cancels the turn's cancellation token and
    /// writes nothing more.
    #[allow(dead_code, reason = "held only for its drop")]
    guard: DropGuard,
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-client-closes
    // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-client-gone
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-cancel
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-client-dropped
}

impl Relay {
    /// The next event of the stream, `None` after the terminal event.
    async fn next_event(&mut self) -> Option<Event> {
        if self.finished {
            return None;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-first
        // `stream_started` first, exactly once
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-first
        let received = if self.content_seen {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-keepalive
            // after the first content event no `ping` is sent: the HTTP server keeps the
            // connection alive with SSE comment lines
            self.events.recv().await
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-keepalive
        } else {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-ping-phase
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-ping
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-ping
            // a `ping` after each interval of idle time; every event restarts the timer
            match tokio::time::timeout(self.ping_interval, self.events.recv()).await {
                Ok(received) => received,
                Err(_idle) => return Some(ping_event()),
            }
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-ping
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-ping
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-ping-phase
        };
        let Some(event) = received else {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-interrupted
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-emit-interrupted
            // the provider task ended without a terminal event while the client is still
            // connected (the CAS was lost to the orphan watchdog, or a panic): the
            // synthesized error matches the `failed` state the winner committed
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-terminal-early
            self.finished = true;
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-terminal-early
            return Some(error_event(
                StreamErrorCode::StreamInterrupted,
                generic_message(StreamErrorCode::StreamInterrupted),
            ));
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-emit-interrupted
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-interrupted
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-content
        match &event {
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-to-streaming
            // the first `delta` or `tool` event ends the ping phase
            RelayEvent::Delta(_) | RelayEvent::Tool(_) => self.content_seen = true,
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-to-streaming
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-content
            // at most one `citations` event, after the last `delta` and before the terminal
            RelayEvent::Citations(_) => {}
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-content
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-close
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-terminal
            // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-close
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-stream
            // exactly one terminal event; the connection closes right after it
            RelayEvent::Done(_) | RelayEvent::Error(_) => self.finished = true,
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-stream
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-close
            // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-terminal
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-close
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-content
        Some(relay_wire(event))
    }
}

/// The SSE response of a turn whose real provider task runs (slice S2-provider-path): HTTP 200,
/// `Content-Type: text/event-stream`, `Cache-Control: no-cache`, no `Connection: keep-alive`
/// header and no compression. `stream_started` first, a `ping` after each `ping_interval` of
/// idle time until the first `delta` or `tool` event (the timer is reset by every event),
/// every provider-driven event flushed as it arrives, exactly one terminal event followed by
/// the close of the connection, and a synthesized `error{code: "stream_interrupted"}` when the
/// channel ends without a terminal event while the client is still connected. Dropping the
/// body (the client went away) cancels the turn's token through `guard` and writes nothing
/// more.
#[must_use]
pub fn relay_response(started: StartedTurn, guard: DropGuard, ping_interval: Duration) -> Response {
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-stream-started
    // @cpt-begin:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-to-awaiting
    // `stream_started`: the request id, the pre-allocated assistant message id, `is_new_turn`
    // `true` and the applied thread summary
    let plan = &started.plan;
    let first = wire_event(StreamEvent::StreamStarted(StreamStarted {
        request_id: plan.request_id,
        message_id: plan.assistant_message_id,
        is_new_turn: true,
        thread_summary_token_estimate: plan.context.summary_token_estimate,
    }));
    // @cpt-end:cpt-cf-mini-chat-state-message-streaming-sse-stream:p1:inst-sse-to-awaiting
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-stream-started
    let relay = Relay {
        first: Some(first),
        events: started.events,
        ping_interval,
        content_seen: false,
        finished: false,
        guard,
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-headers
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-write
    // `Content-Type: text/event-stream` and `Cache-Control: no-cache` from the SSE response, no
    // `Connection: keep-alive` header, no compression layer and no body buffering; each event
    // is written as `event: <name>` and `data: <JSON>` and flushed as it is produced
    let body = stream::unfold(relay, |mut relay| async move {
        relay
            .next_event()
            .await
            .map(|event| (Ok::<Event, Infallible>(event), relay))
    });
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-return
    Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE_INTERVAL))
        .into_response()
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-write
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-headers
}
