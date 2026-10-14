//! The failures of a retry or an edit after the mutation committed, over the real router, SQLite,
//! the real quota service and a mock OAGW: a context that no longer fits (400
//! `CONTEXT_BUDGET_EXCEEDED`), an unresolvable provider (500), a reserve re-check over the limit
//! (429 scope `tokens`), each answering a JSON Problem (never SSE) with the replacement turn
//! `failed` (`context_length_exceeded`, `turn_setup_failed`, `quota_exceeded`), no usage event, no
//! settlement and no turn audit event, the turn status reporting `error` with the stored code, the
//! chat accepting a new turn at once, and a connection dropped after the reserve commit ending the
//! turn `cancelled` without a provider request.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flows
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn` (`inst-retry-setup`,
//! `inst-retry-setup-failed`, `inst-retry-return-setup`, `inst-retry-dropped`,
//! `inst-retry-return-dropped`) and `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn`
//! (`inst-edit-spawn-setup`, `inst-edit-setup`, `inst-edit-dropped`); algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-post-commit-setup` (`inst-pcs-context-rejected`,
//! `inst-pcs-return-context`, `inst-pcs-provider-failed`, `inst-pcs-return-provider`,
//! `inst-pcs-over-limit`, `inst-pcs-return-429`, `inst-pcs-reserve-failed`,
//! `inst-pcs-return-reserve-500`, `inst-pcs-before-provider`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-unstarted-turn-failure` (`inst-uf-cas`,
//! `inst-uf-no-settlement`, `inst-uf-keep-replacement`, `inst-uf-not-sse`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`
//! (`inst-ae-replacement-turn`); state
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup`
//! (`inst-rts-to-failed-setup`, `inst-rts-immutable`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-post-commit-setup`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::USER_A1;
use crate::test_support::chats::{assert_problem, violation};
use crate::test_support::mutations::{
    ArmedHook, ExecSql, assert_internal, fill_daily_reserved_sql, live_messages, mutation_events,
    replacement_of, seed, sse_of, started_ids, turn, turn_by_request, turn_events,
};
use crate::test_support::provider::{MockProvider, Reply, happy_steps};
use crate::test_support::quota::all_quota_rows;
use crate::test_support::send::{
    ReserveGate, SendRig, SendRigOptions, byte_budgets, catalog_of, names, prem_model,
    send_rig_with, std_model, turn_rows, turn_uri,
};

fn quiet_upstream() -> Arc<MockProvider> {
    MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))))
}

fn options_with_std(patch: &Value) -> SendRigOptions {
    SendRigOptions::new().with_snapshot(catalog_of(vec![prem_model(&json!({})), std_model(patch)]))
}

fn tight_context() -> Value {
    json!({ "context_window": 600, "max_input_tokens": 0, "estimation_budgets": byte_budgets() })
}

/// The Problem is JSON (never SSE) and never carries the stored code.
fn assert_json_problem_without_the_code(reply: &crate::test_support::chats::Reply, code: &str) {
    assert!(
        reply.content_type.starts_with("application/problem+json"),
        "no SSE stream is opened: {} {}",
        reply.content_type,
        reply.text
    );
    if code != "quota_exceeded" {
        // the 429 Problem names its own `quota_exceeded` description; the other stored codes
        // never reach the client
        assert!(
            !reply.text.contains(code),
            "the stored code never reaches the client: {}",
            reply.text
        );
    }
}

/// The rows after a setup failure: the replacement turn is `failed` under `code`, the previous
/// turn stays replaced and the replacement user message stays in place.
async fn assert_failed_rows(
    rig: &SendRig,
    chat: &crate::infra::storage::repo_impl::ChatRecord,
    target: &crate::test_support::mutations::SeededTurn,
    code: &str,
) -> Uuid {
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(replacement.state, "failed");
    assert_eq!(replacement.error_code.as_deref(), Some(code));
    assert_eq!(replacement.requester_user_id, Some(USER_A1));
    assert!(replacement.completed_at.is_some());
    assert!(replacement.deleted_at.is_none());
    let previous = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert!(previous.deleted_at.is_some());
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].request_id, Some(replacement.request_id));
    assert_eq!(live[0].role, "user");
    replacement.request_id
}

/// A JSON Problem with the replacement turn failed under `code` and nothing settled or audited.
async fn assert_failed_replacement(
    rig: &SendRig,
    upstream: &MockProvider,
    chat: &crate::infra::storage::repo_impl::ChatRecord,
    target: &crate::test_support::mutations::SeededTurn,
    reply: &crate::test_support::chats::Reply,
    code: &str,
) {
    assert_json_problem_without_the_code(reply, code);
    let replacement_request = assert_failed_rows(rig, chat, target, code).await;
    // no usage event, no settlement, no turn audit event; the mutation audit event stays
    assert!(rig.events().await.is_empty(), "no usage event");
    assert!(turn_events(rig).await.is_empty(), "no turn audit event");
    assert_eq!(mutation_events(rig).await.len(), 1);
    assert_eq!(upstream.call_count(), 0, "no provider request");
    // the turn status reports `error` with the stored code
    let status = rig
        .get(
            &turn_uri(chat.id, replacement_request),
            &crate::test_support::chats::a1(),
        )
        .await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.text);
    let status = status.json();
    assert_eq!(status["state"], "error");
    assert_eq!(status["error_code"], code);
}

// ── a context that no longer fits ────────────────────────────────────────────

#[tokio::test]
async fn a_retry_whose_context_no_longer_fits_is_a_400_and_leaves_a_failed_replacement_turn() {
    // inst-pcs-context-rejected, inst-pcs-return-context, inst-retry-return-setup
    let upstream = quiet_upstream();
    let rig =
        send_rig_with(options_with_std(&tight_context()).with_upstream(upstream.clone())).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn(&"z".repeat(150))).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(
        violation(&body, "context")["reason"],
        "CONTEXT_BUDGET_EXCEEDED"
    );
    assert_failed_replacement(
        &rig,
        &upstream,
        &chat,
        &target,
        &reply,
        "context_length_exceeded",
    )
    .await;
    assert!(
        all_quota_rows(&rig.quota.db).await.is_empty(),
        "no reserve was taken"
    );
}

#[tokio::test]
async fn an_edit_whose_context_no_longer_fits_is_the_same_400() {
    // inst-edit-setup: the new content passes the preflight but not the context budget
    let upstream = quiet_upstream();
    let rig =
        send_rig_with(options_with_std(&tight_context()).with_upstream(upstream.clone())).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("short")).await;
    let reply = rig
        .edit(
            chat.id,
            target.request_id,
            &json!({ "content": "z".repeat(150) }),
        )
        .await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(
        violation(&body, "context")["reason"],
        "CONTEXT_BUDGET_EXCEEDED"
    );
    assert_failed_replacement(
        &rig,
        &upstream,
        &chat,
        &target,
        &reply,
        "context_length_exceeded",
    )
    .await;
}

// ── an unresolvable provider ─────────────────────────────────────────────────

#[tokio::test]
async fn an_unresolvable_provider_is_a_generic_500_and_a_failed_replacement_turn() {
    // inst-pcs-provider-failed, inst-pcs-return-provider
    let upstream = quiet_upstream();
    let rig = send_rig_with(
        options_with_std(&json!({ "provider_id": "ghost_provider" }))
            .with_upstream(upstream.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_internal(&reply);
    assert!(
        !body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("ghost_provider"),
        "{body}"
    );
    assert_failed_replacement(&rig, &upstream, &chat, &target, &reply, "turn_setup_failed").await;
    assert!(all_quota_rows(&rig.quota.db).await.is_empty());
}

// ── the reserve re-check ─────────────────────────────────────────────────────

#[tokio::test]
async fn a_reserve_that_fills_the_quota_after_the_preflight_is_a_429_and_a_failed_replacement_turn()
{
    // inst-pcs-over-limit, inst-pcs-return-429: a concurrent reserve took the room
    let upstream = quiet_upstream();
    let hook = Arc::new(ExecSql(vec![fill_daily_reserved_sql(60_000_000)]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_commit_hook(hook),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(
        body["context"]["violations"][0]["description"],
        "quota_exceeded"
    );
    assert_failed_replacement(&rig, &upstream, &chat, &target, &reply, "quota_exceeded").await;
    // the failed reserve rolled back: only the concurrent turn's reserve remains
    let rows = all_quota_rows(&rig.quota.db).await;
    assert_eq!(
        rows.iter().map(|r| r.reserved).sum::<i64>(),
        60_000_000,
        "{rows:?}"
    );
}

#[tokio::test]
async fn an_edit_that_loses_the_reserve_re_check_is_the_same_429() {
    let upstream = quiet_upstream();
    let hook = Arc::new(ExecSql(vec![fill_daily_reserved_sql(60_000_000)]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_commit_hook(hook),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let reply = rig
        .edit(chat.id, target.request_id, &json!({ "content": "changed" }))
        .await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_failed_replacement(&rig, &upstream, &chat, &target, &reply, "quota_exceeded").await;
}

// ── what the client can do next ──────────────────────────────────────────────

#[tokio::test]
async fn the_chat_accepts_a_new_turn_at_once_after_a_setup_failure() {
    // inst-uf-keep-replacement: nothing stays `running`
    let upstream = quiet_upstream();
    let rig =
        send_rig_with(options_with_std(&tight_context()).with_upstream(upstream.clone())).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn(&"z".repeat(150))).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        turn_rows(&rig.quota.db, chat.id)
            .await
            .iter()
            .all(|row| row.state != "running")
    );
    let next = rig.stream(chat.id, &json!({ "content": "hi" })).await;
    let frames = sse_of(&next);
    assert_eq!(names(&frames).last(), Some(&"done"), "{}", next.text);
}

#[tokio::test]
async fn the_failed_replacement_turn_can_be_deleted_by_the_client() {
    // inst-uf-keep-replacement: the client can retry, edit or delete the failed replacement turn
    let upstream = quiet_upstream();
    let rig = send_rig_with(
        options_with_std(&json!({ "provider_id": "ghost_provider" }))
            .with_upstream(upstream.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    // the messages list shows the new user message with the new request id
    let items = rig.list_messages(chat.id).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["request_id"], replacement.request_id.to_string());
    assert_eq!(items[0]["content"], "question");
    let deleted = rig.delete_turn(chat.id, replacement.request_id).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);
    assert!(rig.list_messages(chat.id).await.is_empty());
}

#[tokio::test]
async fn the_failed_replacement_turn_can_be_edited_with_another_content() {
    // the edit of the failed replacement turn is a normal edit of the latest terminal turn
    let upstream = quiet_upstream();
    let rig =
        send_rig_with(options_with_std(&tight_context()).with_upstream(upstream.clone())).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn(&"z".repeat(150))).await;
    rig.retry(chat.id, target.request_id).await;
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    let reply = rig
        .edit(
            chat.id,
            replacement.request_id,
            &json!({ "content": "short" }),
        )
        .await;
    let frames = sse_of(&reply);
    assert_eq!(names(&frames).last(), Some(&"done"), "{}", reply.text);
    let (newest, _) = started_ids(&frames);
    assert_ne!(newest, replacement.request_id);
    let old = turn_by_request(&rig.quota.db, chat.id, replacement.request_id).await;
    assert_eq!(old.replaced_by_request_id, Some(newest));
}

// ── a disconnect after the reserve commit ────────────────────────────────────

#[tokio::test]
async fn a_handler_dropped_after_the_reserve_commit_ends_the_replacement_turn_cancelled() {
    // inst-retry-dropped, inst-retry-return-dropped: estimated settlement, one `aborted` usage
    // event, one `turn_failed` audit event and no provider request
    let gate = Arc::new(ReserveGate::default());
    let upstream = quiet_upstream();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_gate(Arc::clone(&gate)),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;

    // the setup parks right after the reserve committed; the handler future is dropped there
    let request = rig.retry(chat.id, target.request_id);
    tokio::select! {
        _ = request => panic!("the setup must be parked after the reserve commit"),
        () = gate.committed.notified() => {}
    }
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(replacement.state, "running");
    assert!(
        replacement.reserved_credits_micro.is_some(),
        "the reserve is committed"
    );

    // the detached setup resumes, observes the cancelled token and finalizes the turn
    gate.release.notify_one();
    let settled = rig
        .wait_turn_terminal(chat.id, replacement.request_id)
        .await;
    assert_eq!(settled.state, "cancelled");
    assert_eq!(settled.error_code, None);
    assert_eq!(settled.assistant_message_id, None);
    assert_eq!(upstream.call_count(), 0, "no provider request was sent");
    assert_eq!(rig.provider.call_count(), 0);
    let events = rig.events().await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].terminal_state, "cancelled");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    assert_eq!(turn_events(&rig).await.len(), 1);
    assert_eq!(mutation_events(&rig).await.len(), 1);

    // the chat accepts a new turn at once
    gate.release.notify_one();
    let next = rig.stream(chat.id, &json!({ "content": "again" })).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.text);
}

#[tokio::test]
async fn a_client_that_leaves_the_stream_after_the_first_frames_cancels_the_replacement_turn() {
    // the disconnect semantics of the send path apply to a retry stream
    use crate::test_support::provider::{Gate, Step, delta};
    let gate = Gate::new();
    let upstream = MockProvider::answering(Reply::Sse(vec![
        delta("partial text"),
        Step::Wait(gate.clone()),
    ]));
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream.clone())).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let mut open = rig
        .open_request(rig.retry_request(chat.id, target.request_id))
        .await;
    assert_eq!(open.status, StatusCode::OK);
    let frames = open.until("delta").await;
    let (new_request, _) = started_ids(&frames);
    open.disconnect();
    let settled = rig.wait_turn_terminal(chat.id, new_request).await;
    assert_eq!(settled.state, "cancelled");
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert!(
        live.iter()
            .any(|m| m.role == "assistant" && m.content == "partial text"),
        "the partial answer is persisted: {live:?}"
    );
    gate.open();
}

#[tokio::test]
async fn a_reserve_that_fails_otherwise_is_a_generic_500_and_a_failed_replacement_turn() {
    // inst-pcs-reserve-failed, inst-pcs-return-reserve-500: a database error in the reserve
    // transaction, armed after the mutation committed
    let upstream = quiet_upstream();
    let hook = ArmedHook::new();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_commit_hook(hook.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    hook.arm_after(vec![
        "CREATE TRIGGER fail_reserve_insert BEFORE INSERT ON quota_usage \
         BEGIN SELECT RAISE(ABORT, 'forced reserve failure'); END"
            .to_owned(),
    ]);
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_internal(&reply);
    assert!(
        !body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("reserve"),
        "{body}"
    );
    assert_failed_replacement(&rig, &upstream, &chat, &target, &reply, "turn_setup_failed").await;
    assert!(
        all_quota_rows(&rig.quota.db).await.is_empty(),
        "the reserve rolled back"
    );
}

#[tokio::test]
async fn an_edit_handler_dropped_after_the_reserve_commit_ends_the_turn_cancelled_too() {
    // inst-edit-spawn-setup, inst-edit-dropped: the awaited spawned setup of the edit
    let gate = Arc::new(ReserveGate::default());
    let upstream = quiet_upstream();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_gate(Arc::clone(&gate)),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let body = json!({ "content": "edited" });
    let request = rig.edit(chat.id, target.request_id, &body);
    tokio::select! {
        _ = request => panic!("the setup must be parked after the reserve commit"),
        () = gate.committed.notified() => {}
    }
    let replacement = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(replacement.state, "running");
    gate.release.notify_one();
    let settled = rig
        .wait_turn_terminal(chat.id, replacement.request_id)
        .await;
    assert_eq!(settled.state, "cancelled");
    assert_eq!(upstream.call_count(), 0, "no provider request was sent");
    let events = rig.events().await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(turn_events(&rig).await.len(), 1);
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].content, "edited");
}
