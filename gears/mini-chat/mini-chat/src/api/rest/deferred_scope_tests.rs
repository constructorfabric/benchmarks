//! The deferred and not-implemented scope of the streaming feature stays unimplemented, observed
//! end to end over the real router, SQLite, the real quota service, the real provider task and a
//! mock OAGW: no mid-stream quota abort, no `completion_signal` field, no incremental partial-delta
//! persistence or resume, no attach to a running stream, no persisted citations or
//! `downgrade_reason`, no cross-column CHECK constraints on `chat_turns`, no per-turn `file_search`
//! limit and no replayed citations (the last two are also asserted in `tool_bound_tests` and
//! `stream_relay_tests`).
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-deferred-scope` (algorithms `replay` and
//! `orphan-watchdog` as the places the scope is deferred from).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, assert_problem, message_rows};
use crate::test_support::provider::{
    Gate, MockProvider, Reply, Step, completed, delta, happy_steps,
};
use crate::test_support::send::{
    SendRig, TurnSeedFull, names, parse_sse, seed_turn_full, send_rig, stream_rig, turn_rows,
    turn_uri,
};

async fn streamed(reply: Reply) -> (SendRig, ChatRecord, Arc<MockProvider>) {
    let upstream = MockProvider::answering(reply);
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    (rig, chat, upstream)
}

#[tokio::test]
async fn a_quota_that_runs_out_mid_stream_does_not_abort_the_stream() {
    let gate = Gate::new();
    let steps = vec![
        delta("one "),
        Step::Wait(gate.clone()),
        delta("two"),
        completed("one two", (10, 5)),
    ];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let mut open = rig.open_stream(chat.id, &json!({ "content": "q" })).await;
    open.until("delta").await;
    // the quota is gone while the stream runs: a new send would be rejected ...
    rig.gateway.set_limits((1, 1), (1, 1));
    // ... but the running stream is never cut
    gate.open();
    let frames = open.rest().await;
    assert_eq!(names(&frames), vec!["delta", "done"], "{frames:?}");
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(
        upstream.aborted_streams(),
        0,
        "the provider stream was cancelled"
    );
    // the next send meets the exhausted quota before any stream
    let next = rig.stream(chat.id, &json!({ "content": "q2" })).await;
    assert_problem(&next, 429, "resource_exhausted");
}

#[tokio::test]
async fn no_event_carries_a_completion_signal() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["a", "b"], (3, 2)))).await;
    let request_id = Uuid::new_v4();
    let body = json!({ "content": "q", "request_id": request_id.to_string() });
    let first = rig.stream(chat.id, &body).await;
    let replay = rig.stream(chat.id, &body).await;
    for reply in [&first, &replay] {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert!(!reply.text.contains("completion_signal"), "{}", reply.text);
    }
    // the error path too
    let (rig, chat, _upstream) =
        streamed(Reply::http(500, &json!({ "error": { "message": "boom" } }))).await;
    let failed = rig.stream(chat.id, &json!({ "content": "q" })).await;
    assert!(
        !failed.text.contains("completion_signal"),
        "{}",
        failed.text
    );
    // and the turn status
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    let status = rig.get(&turn_uri(chat.id, turn.request_id), &a1()).await;
    assert!(
        !status.text.contains("completion_signal"),
        "{}",
        status.text
    );
}

#[tokio::test]
async fn partial_text_is_not_persisted_while_the_stream_runs() {
    let gate = Gate::new();
    let steps = vec![
        delta("one "),
        delta("two "),
        Step::Wait(gate.clone()),
        completed("one two three", (1, 1)),
    ];
    let (rig, chat, _upstream) = streamed(Reply::Sse(steps)).await;
    let mut open = rig.open_stream(chat.id, &json!({ "content": "q" })).await;
    open.until("delta").await;
    open.until("delta").await;
    // two deltas were relayed: nothing of the answer is stored, only the user message
    let rows = message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].role, "user");
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "running");
    assert_eq!(turn.assistant_message_id, None);
    gate.open();
    open.rest().await;
}

#[tokio::test]
async fn a_failure_after_deltas_leaves_nothing_to_resume() {
    let (rig, chat, upstream) = streamed(Reply::Sse(vec![delta("lost"), Step::Break])).await;
    let request_id = Uuid::new_v4();
    let body = json!({ "content": "q", "request_id": request_id.to_string() });
    let reply = rig.stream(chat.id, &body).await;
    assert_eq!(names(&parse_sse(&reply.text)).last(), Some(&"error"));
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    // no resume: the same request id is a conflict and no provider call is made for it
    let again = rig.stream(chat.id, &body).await;
    let problem = assert_problem(&again, 409, "aborted");
    assert_eq!(problem["context"]["reason"], "request_id_conflict");
    assert_eq!(upstream.call_count(), 1);
}

#[tokio::test]
async fn a_running_stream_cannot_be_attached_to() {
    let gate = Gate::new();
    let steps = vec![delta("a"), Step::Wait(gate.clone()), completed("a", (1, 1))];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let request_id = Uuid::new_v4();
    let body = json!({ "content": "q", "request_id": request_id.to_string() });
    let mut open = rig.open_stream(chat.id, &body).await;
    open.until("delta").await;
    for attempt in [&body, &json!({ "content": "q" })] {
        let second = rig.stream(chat.id, attempt).await;
        assert_eq!(second.status, StatusCode::CONFLICT, "{}", second.text);
        assert!(
            second.content_type.starts_with("application/problem+json"),
            "no second stream: {}",
            second.content_type
        );
    }
    assert_eq!(upstream.call_count(), 1);
    gate.open();
    open.rest().await;
}

#[tokio::test]
async fn citations_and_the_downgrade_reason_are_not_persisted() {
    let (rig, _chat, _upstream) = streamed(Reply::Sse(happy_steps(&["a"], (1, 1)))).await;
    for (table, forbidden) in [
        ("messages", "citation"),
        ("messages", "downgrade"),
        ("chat_turns", "citation"),
        ("chat_turns", "downgrade"),
    ] {
        let found = crate::test_support::raw_i64(
            &rig.quota.db.raw,
            &format!(
                "SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name LIKE '%{forbidden}%'"
            ),
        )
        .await;
        assert_eq!(found, 0, "`{table}` has a `{forbidden}` column");
    }
}

#[tokio::test]
async fn chat_turns_has_no_cross_column_check_constraint() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("running")).await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("completed")).await;
    // rows that a cross-column CHECK of `state` against `completed_at` and `last_progress_at`
    // would refuse are accepted by the schema: the repositories keep the columns consistent
    for sql in [
        "UPDATE chat_turns SET completed_at = '2026-02-28T10:00:00Z' WHERE state = 'running'",
        "UPDATE chat_turns SET completed_at = NULL WHERE state = 'completed'",
        "UPDATE chat_turns SET last_progress_at = '2026-02-28T10:00:00Z' WHERE state = 'completed'",
        "UPDATE chat_turns SET last_progress_at = NULL WHERE state = 'running'",
    ] {
        crate::test_support::raw_exec(&rig.db.raw, sql)
            .await
            .unwrap_or_else(|error| panic!("a cross-column CHECK refused `{sql}`: {error}"));
    }
    // the updates reached the rows: the table now holds both inconsistent shapes
    let inconsistent = crate::test_support::raw_i64(
        &rig.db.raw,
        "SELECT COUNT(*) FROM chat_turns WHERE (state = 'completed' AND completed_at IS NULL) \
         OR (state = 'running' AND completed_at IS NOT NULL)",
    )
    .await;
    assert_eq!(inconsistent, 2);
}
