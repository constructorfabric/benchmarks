#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! SSE relay tests (DESIGN section 3.3 "`event: ping`", "SSE Event
//! Ordering", "SSE stream close rules").

use std::time::Duration;

use axum::body::to_bytes;
use axum::http::header;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::*;
use crate::domain::quota_service::QuotaDecisionKind;
use crate::domain::stream::events::{Done, DoneUsage, ToolActivity, ToolPhase};

const PING: Duration = Duration::from_millis(40);

fn started() -> StreamStarted {
    StreamStarted {
        request_id: Uuid::from_u128(1),
        message_id: Uuid::from_u128(2),
        is_new_turn: true,
        thread_summary_applied: None,
    }
}

fn done() -> StreamEvent {
    StreamEvent::Done(Box::new(Done {
        usage: DoneUsage {
            input_tokens: 3,
            output_tokens: 4,
        },
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        quota_decision: QuotaDecisionKind::Allow,
        downgrade_from: None,
        downgrade_reason: None,
        quota_warnings: None,
    }))
}

fn live(capacity: usize) -> (GuardedLive, mpsc::Sender<StreamEvent>, CancellationToken) {
    let (tx, rx) = mpsc::channel(capacity);
    let cancel = CancellationToken::new();
    let guarded = GuardedLive::new(LiveStream {
        first: started(),
        rx,
        cancel: cancel.clone(),
    });
    (guarded, tx, cancel)
}

async fn collect(live: GuardedLive) -> Vec<&'static str> {
    live_events(live, PING)
        .map(|event| event.name())
        .collect()
        .await
}

#[tokio::test]
async fn pings_only_between_stream_started_and_the_first_content_event() {
    let (live, tx, cancel) = live(8);
    let producer = tokio::spawn(async move {
        tokio::time::sleep(PING * 3).await;
        tx.send(StreamEvent::text("a")).await.unwrap();
        tokio::time::sleep(PING * 3).await;
        tx.send(StreamEvent::text("b")).await.unwrap();
        tx.send(done()).await.unwrap();
        // The relay closes after `done` even though the sender is alive.
        tokio::time::sleep(PING * 3).await;
        drop(tx);
    });

    let names = collect(live).await;
    producer.await.unwrap();

    assert_eq!(names[0], "stream_started");
    let first_delta = names.iter().position(|name| *name == "delta").unwrap();
    assert!(first_delta >= 2, "expected pings before content: {names:?}");
    assert!(names[1..first_delta].iter().all(|name| *name == "ping"));
    assert_eq!(&names[first_delta..], ["delta", "delta", "done"]);
    assert!(!cancel.is_cancelled(), "a finished turn is not cancelled");
}

#[tokio::test]
async fn a_tool_event_also_stops_the_pings() {
    let (live, tx, _cancel) = live(8);
    let producer = tokio::spawn(async move {
        tx.send(StreamEvent::Tool(ToolActivity {
            phase: ToolPhase::Start,
            name: "web_search".to_owned(),
            details: serde_json::json!({}),
        }))
        .await
        .unwrap();
        tokio::time::sleep(PING * 3).await;
        tx.send(done()).await.unwrap();
    });

    let names = collect(live).await;
    producer.await.unwrap();

    assert_eq!(names, ["stream_started", "tool", "done"]);
}

#[tokio::test]
async fn closed_channel_without_terminal_is_stream_interrupted() {
    let (live, tx, cancel) = live(8);
    tx.send(StreamEvent::text("partial")).await.unwrap();
    drop(tx);

    let events: Vec<MiniChatSseEvent> = live_events(live, PING).collect().await;

    let names: Vec<_> = events.iter().map(MiniChatSseEvent::name).collect();
    assert_eq!(names, ["stream_started", "delta", "error"]);
    let MiniChatSseEvent::Error(error) = &events[2] else {
        panic!("expected error: {events:?}");
    };
    assert_eq!(error.code, "stream_interrupted");
    assert!(!cancel.is_cancelled());
}

#[tokio::test]
async fn dropping_the_relay_before_the_terminal_event_cancels_the_turn() {
    let (live, tx, cancel) = live(8);
    tx.send(StreamEvent::text("a")).await.unwrap();
    let mut events = live_events(live, PING).boxed();
    assert_eq!(events.next().await.unwrap().name(), "stream_started");
    assert_eq!(events.next().await.unwrap().name(), "delta");

    drop(events);

    assert!(cancel.is_cancelled());
    assert!(
        tx.is_closed(),
        "the event channel is dropped with the relay"
    );
}

#[tokio::test]
async fn dropping_an_unsent_live_start_cancels_the_turn() {
    let (live, _tx, cancel) = live(1);
    drop(Started::Live(live));
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn sse_response_has_streaming_headers_and_event_lines() {
    let (live, tx, _cancel) = live(8);
    tx.send(StreamEvent::text("hi")).await.unwrap();
    tx.send(done()).await.unwrap();

    let response = relay(Started::Live(live), PING);

    let headers = response.headers();
    assert_eq!(headers[header::CONTENT_TYPE], "text/event-stream");
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
    assert_eq!(headers["x-accel-buffering"], "no");
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        text.starts_with(
            "event: stream_started\ndata: {\"request_id\":\"00000000-0000-0000-0000-000000000001\",\
             \"message_id\":\"00000000-0000-0000-0000-000000000002\",\"is_new_turn\":true}\n\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("event: delta\ndata: {\"type\":\"text\",\"content\":\"hi\"}\n\n"),
        "{text}"
    );
    assert!(
        text.ends_with(
            "event: done\ndata: {\"usage\":{\"input_tokens\":3,\"output_tokens\":4},\
             \"effective_model\":\"m\",\"selected_model\":\"m\",\"quota_decision\":\"allow\"}\n\n"
        ),
        "{text}"
    );
}

#[tokio::test]
async fn replay_is_sent_as_is() {
    let replay = vec![
        StreamEvent::StreamStarted(StreamStarted {
            is_new_turn: false,
            ..started()
        }),
        StreamEvent::text("stored answer"),
        done(),
    ];

    let response = relay(Started::Replay(replay), PING);

    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    let names: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("event: "))
        .collect();
    assert_eq!(names, ["stream_started", "delta", "done"]);
    assert!(text.contains("\"is_new_turn\":false"), "{text}");
}
