//! The exits after the reserve transaction of `POST /v1/chats/{id}/messages:stream` over the real
//! router, SQLite and the real quota service: a provider failure of the real provider task ends the
//! turn `failed` so the chat accepts the next send, and a handler future dropped after the
//! reserve committed still ends the turn `cancelled` and releases the reserve.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming` (the first test was
//! updated by slice S2-provider-path from the placeholder provider task to the real one): flows
//! `send-message` (`inst-send-dropped-before-provider`, `inst-send-dropped-finalize`) and
//! `disconnect` (`inst-dc-before-provider`); algorithm `reserve-transaction`; definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-reserve-transaction`,
//! `cpt-cf-mini-chat-dod-message-streaming-cancellation` and
//! `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;

use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::message_rows;
use crate::test_support::provider::{MockProvider, Reply};
use crate::test_support::send::{
    ProviderMode, ReserveGate, SendRig, SendRigOptions, footprint, parse_sse, send_rig_with,
    turn_rows,
};

/// Wait until the chat has no `running` turn left (the detached setup task finished).
async fn wait_until_settled(rig: &SendRig, chat: &ChatRecord) {
    for _ in 0..500 {
        let turns = turn_rows(&rig.quota.db, chat.id).await;
        if !turns.is_empty() && turns.iter().all(|turn| turn.state != "running") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("a turn is still running");
}

#[tokio::test]
async fn a_provider_failure_fails_the_turn_so_the_next_send_is_accepted() {
    // updated from the S1 placeholder provider task: the real task against a failing provider
    let upstream = MockProvider::answering(Reply::http(
        500,
        &json!({ "error": { "message": "the provider is not available" } }),
    ));
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream)).await;
    let chat = rig.std_chat().await;

    let first = rig.stream(chat.id, &json!({ "content": "one" })).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    // the response states what the stored turn says: `stream_started`, then `error`
    let frames = parse_sse(&first.text);
    let names: Vec<&str> = frames.iter().map(|f| f.event.as_str()).collect();
    assert_eq!(names, vec!["stream_started", "error"], "{}", first.text);
    assert_eq!(frames[1].data["code"], "provider_error");
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, "failed");
    assert_eq!(turns[0].error_code.as_deref(), Some("provider_error"));
    assert_eq!(turns[0].assistant_message_id, None);
    // the reserve is settled on the estimated path with one `failed` usage event
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
    let events = rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].terminal_state, "failed");
    assert_eq!(events[0].billing_outcome, "failed");
    assert_eq!(events[0].settlement_method, "estimated");

    // no manual finishing: a new `request_id` is accepted, not a 409 `turn_already_running`
    let second = rig.stream(chat.id, &json!({ "content": "two" })).await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.text);
    assert!(
        second.content_type.starts_with("text/event-stream"),
        "{}",
        second.content_type
    );
    let states: Vec<String> = turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .map(|turn| turn.state)
        .collect();
    assert_eq!(states, vec!["failed", "failed"]);
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
}

#[tokio::test]
async fn a_provider_task_that_cannot_start_is_a_500_and_leaves_no_running_turn() {
    let rig = send_rig_with(SendRigOptions::new().with_provider(ProviderMode::Failing)).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &json!({ "content": "one" })).await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, "failed");
    assert_eq!(turns[0].error_code.as_deref(), Some("provider_error"));
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
}

#[tokio::test]
async fn a_handler_dropped_after_the_reserve_commit_still_ends_the_turn_cancelled() {
    let gate = Arc::new(ReserveGate::default());
    let rig = send_rig_with(SendRigOptions::new().with_gate(Arc::clone(&gate))).await;
    let chat = rig.std_chat().await;

    // the setup parks right after the reserve committed; the handler future is dropped there
    // (the client went away before the stream opened), which cancels the turn's token
    let body = json!({ "content": "hi" });
    let request = rig.stream(chat.id, &body);
    tokio::select! {
        _ = request => panic!("the setup must be parked after the reserve commit"),
        () = gate.committed.notified() => {}
    }
    let reserved = footprint(&rig.quota.db, &chat).await.reserved_credits;
    assert!(reserved > 0, "the reserve is committed: {reserved}");
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].state, "running");

    // the detached setup task resumes, observes the cancelled token and finalizes the turn
    gate.release.notify_one();
    wait_until_settled(&rig, &chat).await;

    assert_eq!(rig.provider.call_count(), 0, "no provider request was sent");
    let turns = turn_rows(&rig.quota.db, chat.id).await;
    assert_eq!(turns[0].state, "cancelled");
    assert_eq!(turns[0].error_code, None);
    assert_eq!(turns[0].assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
    let events = rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].terminal_state, "cancelled");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");

    // the chat accepts a new `request_id` at once
    gate.release.notify_one();
    let next = rig.stream(chat.id, &json!({ "content": "again" })).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.text);
}
