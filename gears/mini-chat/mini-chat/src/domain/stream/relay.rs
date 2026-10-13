//! The live stream of a new turn: `stream_started`, then whatever the provider task sends.
//!
//! The provider task runs on its own (`tokio::spawn`) and writes into a bounded channel of
//! `streaming.sse_channel_capacity` events. The returned stream owns a drop guard of the turn's
//! cancellation token: when the HTTP layer drops it (client disconnect), the provider task is
//! cancelled and finalizes the turn as `cancelled`.
//!
//! [`with_pings`] adds the keepalive `ping`s before the first content event and the synthesized
//! `error{stream_interrupted}` when the provider task ends without a terminal event (CAS lost
//! to another finalizer, or a panic) — ADR-0010.

use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, StreamExt as _, stream};
use opentelemetry::KeyValue;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use super::events::{EventStream, StreamEvent};
use super::{StreamService, TurnContext, provider_task};
use crate::infra::llm::{ChatAdapter, ProviderRequest};

/// SSE code sent when the provider task ended without a terminal event.
const STREAM_INTERRUPTED: &str = "stream_interrupted";
const STREAM_INTERRUPTED_MESSAGE: &str = "The stream ended unexpectedly";

/// Starts the provider task of `turn` and returns the stream relayed to the client.
#[must_use]
pub fn spawn_turn(
    svc: StreamService,
    turn: TurnContext,
    adapter: Arc<dyn ChatAdapter>,
    request: ProviderRequest,
) -> EventStream {
    let capacity = usize::from(svc.cfg.streaming.sse_channel_capacity).max(1);
    let ping_every = Duration::from_secs(u64::from(svc.cfg.streaming.sse_ping_interval_seconds));
    let (tx, rx) = mpsc::channel(capacity);
    let cancel = CancellationToken::new();
    let started = StreamEvent::StreamStarted {
        request_id: turn.request_id,
        message_id: turn.assistant_message_id,
        is_new_turn: true,
        thread_summary_applied: turn.thread_summary_applied,
    };
    svc.metrics.stream_started.add(
        1,
        &[
            KeyValue::new("provider", turn.provider_id.clone()),
            KeyValue::new("model", turn.decision.effective_model.id.clone()),
        ],
    );
    tokio::spawn(
        provider_task::run(svc, turn, adapter, request, tx, cancel.clone())
            .instrument(tracing::Span::current()),
    );

    let relayed = stream::unfold(
        (Box::pin(with_pings(rx, ping_every)), cancel.drop_guard()),
        |(mut events, guard)| async move { events.next().await.map(|e| (e, (events, guard))) },
    );
    Box::pin(stream::once(async move { started }).chain(relayed))
}

/// Relay state of [`with_pings`].
struct Relay {
    rx: mpsc::Receiver<StreamEvent>,
    /// A `delta` or `tool` event was relayed: no more pings.
    content_started: bool,
    /// A terminal event was relayed: the stream is over.
    ended: bool,
}

/// The events of `rx` as the client sees them:
/// - `Ping` after every `ping_every` of idle time, only before the first `Delta`/`Tool` (every
///   event restarts the timer);
/// - the stream ends right after a terminal event;
/// - when the channel closes without a terminal event, `Error{stream_interrupted}` is the last
///   event.
pub fn with_pings(
    rx: mpsc::Receiver<StreamEvent>,
    ping_every: Duration,
) -> impl Stream<Item = StreamEvent> {
    let relay = Relay {
        rx,
        content_started: false,
        ended: false,
    };
    stream::unfold(relay, move |mut relay| async move {
        if relay.ended {
            return None;
        }
        let next = if relay.content_started {
            relay.rx.recv().await
        } else {
            tokio::select! {
                biased;
                event = relay.rx.recv() => event,
                () = tokio::time::sleep(ping_every) => return Some((StreamEvent::Ping, relay)),
            }
        };
        let event = next.unwrap_or_else(|| {
            tracing::warn!("provider task ended without a terminal event");
            StreamEvent::Error {
                code: STREAM_INTERRUPTED.to_owned(),
                message: STREAM_INTERRUPTED_MESSAGE.to_owned(),
            }
        });
        relay.content_started |=
            matches!(event, StreamEvent::Delta { .. } | StreamEvent::Tool { .. });
        relay.ended = event.is_terminal();
        Some((event, relay))
    })
}
