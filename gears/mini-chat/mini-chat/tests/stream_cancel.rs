//! Client disconnect during a stream: the turn becomes `cancelled`, the provider
//! request is cancelled (hard cancel), the partial answer is persisted when it
//! is not empty and the chat is released for the next message.
//!
//! Source of truth: DESIGN 3.6 "Streaming Cancellation", 3.3 "SSE stream close
//! rules" (client disconnect before / after the terminal event), 4 "Turn
//! Lifecycle" (state model, Non-Goals: the partial message is a one-time
//! snapshot that takes part in later context), 5.7 "Terminal outcome taxonomy"
//! and "P1 client disconnect rules", the Turn Status API (`cancelled`,
//! `assistant_message_id` only with persisted partial content).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod stream_support;

use serde_json::json;
use stream_support::{Opts, ProviderReply, StreamHarness, assert_conflict, ctx_a};
use uuid::Uuid;

#[tokio::test]
async fn a_disconnect_mid_stream_cancels_the_turn_and_persists_the_partial_answer() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["Hel", "lo", " world"],
        &Opts::default().pause_after(2),
    ));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "greet me", "request_id": rid}),
        )
        .await
        .live();
    let started = live.expect("stream_started").await;
    let message_id = Uuid::parse_str(started.data["message_id"].as_str().unwrap()).unwrap();
    live.next_non_ping().await.expect("first delta");
    live.next_non_ping().await.expect("second delta");
    h.oagw.wait_paused(1).await;

    drop(live); // the client goes away

    let turn = h.wait_turn_state(rid, "cancelled").await;
    h.oagw.wait_dropped_early(1).await; // the provider request was cancelled, not awaited
    assert!(
        turn.completed_at.is_some(),
        "a terminal turn has completed_at"
    );
    assert_eq!(turn.error_code, None, "a cancellation is not an error");
    assert_eq!(turn.assistant_message_id, Some(message_id));

    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].role, "assistant");
    assert_eq!(rows[1].id, message_id, "the id announced by stream_started");
    assert_eq!(
        rows[1].content, "Hello",
        "the accumulated text up to the cancel point"
    );
    assert_eq!(rows[1].request_id, Some(rid));
    assert_eq!(rows[1].model.as_deref(), Some("gpt-premium"));

    let status = h.turn_status(chat, rid, &ctx_a()).await;
    assert_eq!(status.json()["state"], json!("cancelled"));
    assert_eq!(
        status.json()["assistant_message_id"],
        json!(message_id.to_string())
    );
    assert!(status.json().get("error_code").is_none());

    let items = h.listed_messages(chat).await;
    assert_eq!(items[1]["content"], json!("Hello"));
}

#[tokio::test]
async fn a_disconnect_before_any_content_cancels_without_an_assistant_message() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["never seen"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": "q", "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    drop(live);

    let turn = h.wait_turn_state(rid, "cancelled").await;
    h.oagw.wait_dropped_early(1).await;
    assert_eq!(turn.assistant_message_id, None, "nothing to persist");
    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 1, "only the user message");
    assert_eq!(rows[0].role, "user");

    let status = h.turn_status(chat, rid, &ctx_a()).await;
    assert_eq!(status.json()["state"], json!("cancelled"));
    assert!(status.json().get("assistant_message_id").is_none());
}

#[tokio::test]
async fn dropping_the_response_before_reading_anything_does_not_leave_the_turn_running() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["never seen"],
        &Opts::default().pause_after(0),
    ));
    let raw = h
        .open(chat, &ctx_a(), &json!({"content": "q", "request_id": rid}))
        .await;
    assert!(raw.is_sse());
    h.oagw.wait_paused(1).await;
    drop(raw); // the unsent / unread response is dropped

    h.wait_turn_state(rid, "cancelled").await;
    h.oagw.wait_dropped_early(1).await;
}

#[tokio::test]
async fn a_disconnect_seen_as_a_failed_send_under_backpressure_cancels_too() {
    let h = StreamHarness::with(stream_support::StreamOptions {
        config: json!({"streaming": {"sse_channel_capacity": 16}}),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    let chunks: Vec<String> = (0..400).map(|i| format!("w{i} ")).collect();
    let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
    h.oagw.push(ProviderReply::text(&refs));

    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "flood", "request_id": rid}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // the provider task blocks on the full channel
    drop(live);

    let turn = h.wait_turn_state(rid, "cancelled").await;
    assert_eq!(turn.error_code, None);
    let rows = h.messages_in(chat).await;
    let partial = rows
        .iter()
        .find(|m| m.role == "assistant")
        .expect("a partial answer");
    assert!(!partial.content.is_empty());
    assert!(
        chunks.concat().starts_with(&partial.content),
        "the partial answer is a prefix of the full text"
    );
    assert!(
        partial.content.len() < chunks.concat().len(),
        "the stream was cut short"
    );
}

#[tokio::test]
async fn a_disconnect_after_the_terminal_event_changes_nothing() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": "q", "request_id": rid}))
        .await
        .live();
    let events = live.drain().await;
    assert_eq!(events.last().unwrap().event, "done");
    drop(live);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let turn = h.turn_of(rid).await.unwrap();
    assert_eq!(turn.state, "completed");
    assert!(turn.assistant_message_id.is_some());
    assert_eq!(h.oagw.dropped_early(), 0);
    assert_eq!(h.messages_in(chat).await.len(), 2);
}

#[tokio::test]
async fn a_cancelled_turn_frees_the_chat_but_keeps_its_request_id_taken() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["x"],
        &Opts::default().pause_after(0),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": "q", "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(rid, "cancelled").await;

    let resp = h
        .reject(chat, json!({"content": "q", "request_id": rid}))
        .await;
    assert_conflict(&resp, "request_id_conflict");

    let sent = h.say(chat, "a fresh message").await;
    assert_eq!(sent.terminal().event, "done", "the chat is not blocked");
}

#[tokio::test]
async fn the_partial_answer_of_a_cancelled_turn_is_part_of_the_next_context() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["Par", "tial"],
        &Opts::default().pause_after(2),
    ));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "first", "request_id": rid}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.unwrap();
    live.next_non_ping().await.unwrap();
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(rid, "cancelled").await;

    h.say(chat, "second").await;
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec!["user:first", "assistant:Partial", "user:second"]
    );
}
