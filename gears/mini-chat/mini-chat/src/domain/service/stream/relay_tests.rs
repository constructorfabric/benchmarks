#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::unchecked_time_subtraction)]
//! Slice 5: the relay
//! (`cpt-cf-mini-chat-algo-streaming-turns-relay-sse`,
//! `cpt-cf-mini-chat-state-streaming-turns-sse-stream`): ordering, the ping
//! timer on a paused clock, the `stream_interrupted` synthesis and the drop
//! guard.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::preflight::QuotaDecision;
use crate::domain::service::stream::ports::DisconnectStage;
use crate::domain::service::stream::test_support::{RecordingStreamMetrics, StreamMetric};
use crate::domain::service::stream::types::{
    DeltaKind, DoneEvent, ErrorEvent, StreamErrorCode, ToolName, ToolPhase,
};

const PING: Duration = Duration::from_secs(5);

struct Rig {
    tx: mpsc::Sender<StreamEvent>,
    token: CancellationToken,
    metrics: Arc<RecordingStreamMetrics>,
    handle: Option<RelayHandle>,
}

fn rig() -> Rig {
    let (tx, rx) = mpsc::channel(8);
    let token = CancellationToken::new();
    let metrics = RecordingStreamMetrics::new();
    let handle = RelayHandle::new(
        rx,
        token.clone(),
        Arc::clone(&metrics) as Arc<dyn StreamMetrics>,
        PING,
    );
    Rig {
        tx,
        token,
        metrics,
        handle: Some(handle),
    }
}

impl Rig {
    fn stream(&mut self) -> RelayStream {
        self.handle.take().unwrap().into_stream(started())
    }
}

fn started() -> StreamStarted {
    StreamStarted {
        request_id: Uuid::from_u128(1),
        message_id: Uuid::from_u128(2),
        is_new_turn: true,
        thread_summary_applied: None,
    }
}

fn delta(text: &str) -> StreamEvent {
    StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: text.to_owned(),
    }
}

fn tool() -> StreamEvent {
    StreamEvent::Tool {
        phase: ToolPhase::Start,
        name: ToolName::WebSearch,
        details: json!({}),
    }
}

fn done() -> StreamEvent {
    StreamEvent::Done(DoneEvent {
        usage: (1, 2),
        effective_model: "p".to_owned(),
        selected_model: "p".to_owned(),
        quota_decision: QuotaDecision::Allow,
        downgrade_from: None,
        downgrade_reason: None,
        quota_warnings: Some(Vec::new()),
    })
}

fn error(code: StreamErrorCode) -> StreamEvent {
    StreamEvent::Error(ErrorEvent {
        code,
        message: "m".to_owned(),
    })
}

async fn names(stream: &mut RelayStream, count: usize) -> Vec<&'static str> {
    let mut out = Vec::new();
    for _ in 0..count {
        out.push(stream.next().await.expect("an event").name());
    }
    out
}

// ---- ordering and closing -----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn stream_started_comes_first_then_the_channel_events_in_order() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(delta("a")).await.unwrap();
    rig.tx.send(tool()).await.unwrap();
    rig.tx.send(delta("b")).await.unwrap();
    rig.tx.send(done()).await.unwrap();
    assert_eq!(
        stream.next().await,
        Some(StreamEvent::StreamStarted(started()))
    );
    assert_eq!(stream.next().await, Some(delta("a")));
    assert_eq!(stream.next().await, Some(tool()));
    assert_eq!(stream.next().await, Some(delta("b")));
    assert_eq!(stream.next().await, Some(done()));
    assert_eq!(
        stream.next().await,
        None,
        "closed right after the terminal event"
    );
}

#[tokio::test(start_paused = true)]
async fn nothing_follows_a_terminal_event() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx
        .send(error(StreamErrorCode::ProviderError))
        .await
        .unwrap();
    rig.tx.send(delta("late")).await.unwrap();
    rig.tx.send(done()).await.unwrap();
    assert_eq!(names(&mut stream, 2).await, ["stream_started", "error"]);
    assert_eq!(stream.next().await, None);
    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn an_available_event_is_yielded_without_waiting_for_the_timer() {
    let mut rig = rig();
    let mut stream = rig.stream();
    stream.next().await; // stream_started
    rig.tx.send(delta("now")).await.unwrap();
    let before = Instant::now();
    assert_eq!(stream.next().await, Some(delta("now")));
    assert_eq!(
        before.elapsed(),
        Duration::ZERO,
        "no buffering, no timer wait"
    );
}

// ---- pings --------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_ping_follows_every_interval_of_idle_time_before_the_first_content() {
    let mut rig = rig();
    let mut stream = rig.stream();
    stream.next().await; // stream_started
    let start = Instant::now();
    assert_eq!(stream.next().await, Some(StreamEvent::Ping));
    assert_eq!(start.elapsed(), PING);
    assert_eq!(stream.next().await, Some(StreamEvent::Ping));
    assert_eq!(start.elapsed(), PING * 2);
}

#[tokio::test(start_paused = true)]
async fn two_pings_arrive_while_the_provider_is_silent_for_two_intervals() {
    let mut rig = rig();
    let mut stream = rig.stream();
    let tx = rig.tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(PING * 2 + Duration::from_secs(1)).await;
        tx.send(delta("first")).await.unwrap();
        tx.send(done()).await.unwrap();
    });
    assert_eq!(
        names(&mut stream, 5).await,
        ["stream_started", "ping", "ping", "delta", "done"]
    );
}

#[tokio::test(start_paused = true)]
async fn the_ping_timer_restarts_from_the_stream_start_not_from_the_wait() {
    let mut rig = rig();
    let mut stream = rig.stream();
    let start = Instant::now();
    stream.next().await; // stream_started at t0
    let tx = rig.tx.clone();
    tokio::spawn(async move {
        // Content right before the first ping would be due.
        tokio::time::sleep(PING - Duration::from_millis(1)).await;
        tx.send(delta("early")).await.unwrap();
    });
    assert_eq!(stream.next().await, Some(delta("early")));
    assert_eq!(start.elapsed(), PING - Duration::from_millis(1));
}

#[tokio::test(start_paused = true)]
async fn no_ping_after_the_first_delta() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(delta("x")).await.unwrap();
    stream.next().await;
    stream.next().await;
    let idle = tokio::time::timeout(PING * 10, stream.next()).await;
    assert!(
        idle.is_err(),
        "nothing is written while the provider pauses: {idle:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn no_ping_after_the_first_tool_event() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(tool()).await.unwrap();
    stream.next().await;
    stream.next().await;
    assert!(
        tokio::time::timeout(PING * 10, stream.next())
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn a_ping_does_not_count_as_content() {
    let mut rig = rig();
    let mut stream = rig.stream();
    stream.next().await;
    assert_eq!(stream.next().await, Some(StreamEvent::Ping));
    assert_eq!(stream.next().await, Some(StreamEvent::Ping));
    drop(stream);
    assert_eq!(
        rig.metrics.events(),
        vec![
            StreamMetric::CancelRequested,
            StreamMetric::Disconnected(DisconnectStage::BeforeFirstToken)
        ]
    );
}

// ---- stream_interrupted -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_channel_that_closes_without_a_terminal_event_ends_with_stream_interrupted() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(delta("partial")).await.unwrap();
    let tx = std::mem::replace(&mut rig.tx, mpsc::channel(1).0);
    drop(tx);
    assert_eq!(names(&mut stream, 2).await, ["stream_started", "delta"]);
    assert_eq!(
        stream.next().await,
        Some(error(StreamErrorCode::StreamInterrupted).with_message_of_code())
    );
    assert_eq!(stream.next().await, None);
}

#[tokio::test(start_paused = true)]
async fn stream_interrupted_is_also_sent_before_any_content() {
    let mut rig = rig();
    let mut stream = rig.stream();
    let tx = std::mem::replace(&mut rig.tx, mpsc::channel(1).0);
    drop(tx);
    stream.next().await;
    let Some(StreamEvent::Error(event)) = stream.next().await else {
        panic!("error expected");
    };
    assert_eq!(event.code, StreamErrorCode::StreamInterrupted);
    assert!(!event.message.is_empty());
}

trait WithMessage {
    fn with_message_of_code(self) -> StreamEvent;
}

impl WithMessage for StreamEvent {
    /// The expected `stream_interrupted` event: the generic text of its code.
    fn with_message_of_code(self) -> StreamEvent {
        match self {
            Self::Error(event) => Self::Error(ErrorEvent {
                message: event.code.generic_message().to_owned(),
                ..event
            }),
            other => other,
        }
    }
}

// ---- the drop guard -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_before_the_first_delta_is_a_disconnect_before_first_token() {
    let mut rig = rig();
    let stream = rig.stream();
    assert!(!rig.token.is_cancelled());
    drop(stream);
    assert!(rig.token.is_cancelled());
    assert_eq!(
        rig.metrics.events(),
        vec![
            StreamMetric::CancelRequested,
            StreamMetric::Disconnected(DisconnectStage::BeforeFirstToken)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn dropping_the_stream_after_a_delta_is_a_mid_stream_disconnect() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(delta("x")).await.unwrap();
    stream.next().await;
    stream.next().await;
    drop(stream);
    assert!(rig.token.is_cancelled());
    assert_eq!(
        rig.metrics.events(),
        vec![
            StreamMetric::CancelRequested,
            StreamMetric::Disconnected(DisconnectStage::MidStream)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_event_alone_does_not_make_the_disconnect_mid_stream() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(tool()).await.unwrap();
    stream.next().await;
    stream.next().await;
    drop(stream);
    assert_eq!(
        rig.metrics.events().last(),
        Some(&StreamMetric::Disconnected(
            DisconnectStage::BeforeFirstToken
        ))
    );
}

#[tokio::test(start_paused = true)]
async fn a_completed_stream_records_no_disconnect_when_it_is_dropped() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(delta("x")).await.unwrap();
    rig.tx.send(done()).await.unwrap();
    while stream.next().await.is_some() {}
    drop(stream);
    assert!(
        rig.metrics.events().is_empty(),
        "{:?}",
        rig.metrics.events()
    );
    assert!(!rig.token.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn the_guard_is_disarmed_when_the_terminal_event_is_written_not_when_the_end_is_polled() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx.send(done()).await.unwrap();
    stream.next().await;
    assert_eq!(stream.next().await.map(|e| e.name()), Some("done"));
    drop(stream); // dropped before the `None` was ever polled
    assert!(rig.metrics.events().is_empty());
    assert!(!rig.token.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn an_error_event_disarms_the_guard_too() {
    let mut rig = rig();
    let mut stream = rig.stream();
    rig.tx
        .send(error(StreamErrorCode::ProviderError))
        .await
        .unwrap();
    stream.next().await;
    stream.next().await;
    drop(stream);
    assert!(rig.metrics.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_synthesized_stream_interrupted_disarms_the_guard() {
    let mut rig = rig();
    let mut stream = rig.stream();
    let tx = std::mem::replace(&mut rig.tx, mpsc::channel(1).0);
    drop(tx);
    stream.next().await;
    stream.next().await;
    drop(stream);
    assert!(rig.metrics.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_handle_dropped_before_the_relay_started_cancels_the_turn() {
    // The setup result was never delivered (the handler was gone).
    let mut rig = rig();
    let handle = rig.handle.take().unwrap();
    drop(handle);
    assert!(rig.token.is_cancelled());
    assert_eq!(
        rig.metrics.events(),
        vec![
            StreamMetric::CancelRequested,
            StreamMetric::Disconnected(DisconnectStage::BeforeFirstToken)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn the_disconnect_is_recorded_once() {
    let mut rig = rig();
    let stream = rig.stream();
    drop(stream);
    assert_eq!(
        rig.metrics.count(|m| *m == StreamMetric::CancelRequested),
        1
    );
    assert_eq!(
        rig.metrics
            .count(|m| matches!(m, StreamMetric::Disconnected(_))),
        1
    );
}

#[test]
fn the_guard_can_be_disarmed_and_notes_deltas() {
    let token = CancellationToken::new();
    let metrics = RecordingStreamMetrics::new();
    let mut guard = DisconnectGuard::new(
        token.clone(),
        Arc::clone(&metrics) as Arc<dyn StreamMetrics>,
    );
    guard.note_delta();
    guard.disarm();
    drop(guard);
    assert!(metrics.events().is_empty());
    assert!(!token.is_cancelled());
}
