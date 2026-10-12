#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use serde_json::{Value, json};
use time::macros::datetime;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::sse_response;
use crate::domain::quota::QuotaWarning;
use crate::domain::stream::{
    CitationItem, CitationSource, DeltaKind, DonePayload, EventStream, StreamEvent, ToolPhase,
};

fn channel(ping: Duration) -> (mpsc::Sender<StreamEvent>, EventStream) {
    let (tx, rx) = mpsc::channel(16);
    (
        tx,
        EventStream {
            events: rx,
            ping_interval: ping,
        },
    )
}

fn started() -> StreamEvent {
    StreamEvent::Started {
        request_id: Uuid::nil(),
        message_id: Uuid::nil(),
        is_new_turn: true,
        thread_summary_tokens: None,
    }
}

fn done() -> StreamEvent {
    StreamEvent::Done(DonePayload {
        input_tokens: 10,
        output_tokens: 5,
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        quota_decision: "allow",
        downgrade_from: None,
        downgrade_reason: None,
        quota_warnings: Some(Vec::new()),
    })
}

/// The `(event, data)` pairs of the response body.
async fn read(stream: EventStream) -> Vec<(String, Value)> {
    let response = sse_response(stream);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("no-cache")
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .split("\n\n")
        .filter(|frame| !frame.trim().is_empty() && !frame.starts_with(':'))
        .map(|frame| {
            let name = frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .unwrap()
                .to_owned();
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            (name, serde_json::from_str(data).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn every_event_has_its_wire_shape() {
    let (tx, stream) = channel(Duration::from_secs(60));
    tx.send(StreamEvent::Started {
        request_id: Uuid::nil(),
        message_id: Uuid::nil(),
        is_new_turn: false,
        thread_summary_tokens: Some(42),
    })
    .await
    .unwrap();
    tx.send(StreamEvent::Delta {
        kind: DeltaKind::Reasoning,
        content: "think".to_owned(),
    })
    .await
    .unwrap();
    tx.send(StreamEvent::Tool {
        phase: ToolPhase::Done,
        name: "file_search".to_owned(),
        details: json!({ "files_searched": 0 }),
    })
    .await
    .unwrap();
    tx.send(StreamEvent::Citations(vec![
        CitationItem {
            source: CitationSource::Web,
            title: "T".to_owned(),
            url: Some("https://a.b".to_owned()),
            attachment_id: None,
            snippet: "s".to_owned(),
            span: Some((1, 2)),
        },
        CitationItem {
            source: CitationSource::File,
            title: "f.pdf".to_owned(),
            url: None,
            attachment_id: Some(Uuid::nil()),
            snippet: String::new(),
            span: None,
        },
    ]))
    .await
    .unwrap();
    tx.send(StreamEvent::Done(DonePayload {
        input_tokens: 1,
        output_tokens: 2,
        effective_model: "e".to_owned(),
        selected_model: "s".to_owned(),
        quota_decision: "downgrade",
        downgrade_from: Some("s".to_owned()),
        downgrade_reason: Some("force_standard_tier"),
        quota_warnings: Some(vec![QuotaWarning {
            tier: "total",
            period: "daily",
            remaining_percentage: 3,
            warning: true,
            exhausted: false,
            next_reset: Some(datetime!(2026-03-01 00:00 UTC)),
        }]),
    }))
    .await
    .unwrap();
    drop(tx);

    let events = read(stream).await;

    assert_eq!(
        events[0].1["thread_summary_applied"],
        json!({ "token_estimate": 42 })
    );
    assert_eq!(events[0].1["is_new_turn"], json!(false));
    assert_eq!(
        events[1],
        (
            "delta".to_owned(),
            json!({ "type": "reasoning", "content": "think" })
        )
    );
    assert_eq!(
        events[2].1,
        json!({ "phase": "done", "name": "file_search", "details": { "files_searched": 0 } })
    );
    assert_eq!(
        events[3].1["items"][0],
        json!({ "source": "web", "title": "T", "url": "https://a.b", "snippet": "s", "span": { "start": 1, "end": 2 } })
    );
    assert_eq!(
        events[3].1["items"][1],
        json!({ "source": "file", "title": "f.pdf", "attachment_id": Uuid::nil().to_string(), "snippet": "" })
    );
    assert_eq!(
        events[4].1["usage"],
        json!({ "input_tokens": 1, "output_tokens": 2 })
    );
    assert_eq!(
        events[4].1["downgrade_reason"],
        json!("force_standard_tier")
    );
    assert_eq!(
        events[4].1["quota_warnings"][0],
        json!({
            "tier": "total", "period": "daily", "remaining_percentage": 3,
            "warning": true, "exhausted": false, "next_reset": "2026-03-01T00:00:00Z"
        })
    );
}

#[tokio::test]
async fn pings_fill_the_idle_time_between_stream_started_and_the_first_content_event() {
    let (tx, stream) = channel(Duration::from_millis(15));
    let producer = tokio::spawn(async move {
        tx.send(started()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.send(StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: "x".to_owned(),
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        tx.send(done()).await.unwrap();
    });

    let events = read(stream).await;
    producer.await.unwrap();

    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names[0], "stream_started");
    assert!(
        names.iter().filter(|name| **name == "ping").count() >= 2,
        "{names:?}"
    );
    let delta = names.iter().position(|name| *name == "delta").unwrap();
    assert!(
        names[..delta].iter().skip(1).all(|name| *name == "ping"),
        "only pings precede the first content: {names:?}"
    );
    assert!(
        names[delta..].iter().all(|name| *name != "ping"),
        "no ping after content started: {names:?}"
    );
    assert_eq!(names.last(), Some(&"done"));
    assert_eq!(events[1].1, json!({}), "a ping carries empty data");
}

#[tokio::test]
async fn no_ping_is_sent_before_stream_started() {
    let (tx, stream) = channel(Duration::from_millis(10));
    let producer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        tx.send(started()).await.unwrap();
        tx.send(done()).await.unwrap();
    });

    let events = read(stream).await;
    producer.await.unwrap();

    assert_eq!(events[0].0, "stream_started");
}

#[tokio::test]
async fn the_stream_ends_right_after_the_terminal_event() {
    let (tx, stream) = channel(Duration::from_secs(60));
    tx.send(started()).await.unwrap();
    tx.send(StreamEvent::Error {
        code: "provider_error".to_owned(),
        message: "boom".to_owned(),
    })
    .await
    .unwrap();
    tx.send(done()).await.unwrap();

    let events = read(stream).await;

    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["stream_started", "error"]);
    assert_eq!(
        events[1].1,
        json!({ "code": "provider_error", "message": "boom" })
    );
}

#[tokio::test]
async fn a_channel_that_closes_without_a_terminal_event_ends_with_stream_interrupted() {
    let (tx, stream) = channel(Duration::from_secs(60));
    tx.send(started()).await.unwrap();
    tx.send(StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: "x".to_owned(),
    })
    .await
    .unwrap();
    drop(tx);

    let events = read(stream).await;

    let (name, data) = events.last().unwrap();
    assert_eq!(name, "error");
    assert_eq!(data["code"], json!("stream_interrupted"));
}
