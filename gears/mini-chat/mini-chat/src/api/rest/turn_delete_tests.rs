//! `DELETE /v1/chats/{id}/turns/{request_id}` over the real router, SQLite and the real quota
//! service: the 204 with no body, the turn soft-deleted without `replaced_by_request_id`, its
//! messages gone from the list and from `message_count`, its turn status 404, the previous turn
//! becoming the latest turn, the replay of its `request_id` as a 409, the chat activity and the
//! quota rows unchanged, one `turn_delete` audit event, no preflight, no reserve, no provider
//! call and no stream, and every rejection (path, admission, preview, commit) leaving nothing
//! behind.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flow
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn` (`inst-del-request`,
//! `inst-del-bad-path`, `inst-del-return-bad-path`, `inst-del-admission`, `inst-del-refused`,
//! `inst-del-return-refusal`, `inst-del-preview`, `inst-del-commit`, `inst-del-commit-rejected`,
//! `inst-del-return-commit`, `inst-del-return-204`); algorithms
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit` (`inst-mc-delete-no-activity`,
//! `inst-mc-payload-too-large`, `inst-mc-return-payload-500`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order` (`inst-mo-exclude-deleted`,
//! `inst-mo-count`, `inst-mo-old-request-id`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`; states
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility` (`inst-tv-to-deleted`,
//! `inst-tv-final`); definitions of done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-delete`
//! and `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use mini_chat_sdk::AuditEvent;
use serde_json::json;
use uuid::Uuid;

use crate::test_support::chats::{a1, a2, assert_chat_not_found, assert_problem, b1, request};
use crate::test_support::mutations::{
    FailingMutationOutbox, assert_authz_denied, assert_conflict, assert_internal,
    assert_not_found_of, assert_state_violation, live_messages, mutation_events, seed, snapshot,
    turn, turn_by_request,
};
use crate::test_support::quota::{all_quota_rows, row, seed_rows};
use crate::test_support::send::{
    SendRig, SendRigOptions, TURN_RESOURCE, chat_model, send_rig, send_rig_with, stream_uri,
    turn_uri,
};
use crate::test_support::{Pdp, USER_A1, USER_A2};

const DAY: &str = "2026-02-28";

async fn two_turns(
    rig: &SendRig,
) -> (
    crate::infra::storage::repo_impl::ChatRecord,
    crate::test_support::mutations::SeededTurn,
    crate::test_support::mutations::SeededTurn,
) {
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    (chat, first, second)
}

// ── the success path ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_delete_is_a_204_with_no_body_and_no_stream() {
    // inst-del-return-204
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    let reply = rig.delete_turn(chat.id, second.request_id).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(reply.text.is_empty(), "no body: {:?}", reply.text);
    assert!(!reply.content_type.starts_with("text/event-stream"));
    assert_eq!(rig.provider.call_count(), 0, "no provider call");
}

#[tokio::test]
async fn the_deleted_turn_has_deleted_at_and_no_replacement() {
    // inst-tv-to-deleted
    let rig = send_rig().await;
    let (chat, first, second) = two_turns(&rig).await;
    rig.delete_turn(chat.id, second.request_id).await;
    let deleted = turn_by_request(&rig.quota.db, chat.id, second.request_id).await;
    assert!(deleted.deleted_at.is_some());
    assert_eq!(deleted.replaced_by_request_id, None);
    assert_eq!(
        deleted.state, "completed",
        "the execution state is untouched"
    );
    let kept = turn_by_request(&rig.quota.db, chat.id, first.request_id).await;
    assert!(kept.deleted_at.is_none());
    assert_eq!(
        crate::test_support::send::turn_rows(&rig.quota.db, chat.id)
            .await
            .len(),
        2,
        "no new turn"
    );
}

#[tokio::test]
async fn the_messages_of_the_deleted_turn_leave_the_list_and_the_message_count() {
    // inst-mo-exclude-deleted, inst-mo-count: two messages for a completed turn
    let rig = send_rig().await;
    let (chat, first, second) = two_turns(&rig).await;
    assert_eq!(rig.message_count(chat.id).await, 4);
    rig.delete_turn(chat.id, second.request_id).await;
    assert_eq!(rig.message_count(chat.id).await, 2);
    let items = rig.list_messages(chat.id).await;
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .all(|i| i["request_id"] == first.request_id.to_string())
    );
    // the rows are kept in storage, soft-deleted
    let rows = crate::test_support::chats::message_rows(&rig.quota.db, chat.id).await;
    assert_eq!(rows.len(), 4);
    assert_eq!(rows.iter().filter(|m| m.deleted_at.is_some()).count(), 2);
}

#[tokio::test]
async fn a_failed_or_a_cancelled_turn_without_text_removes_one_message() {
    // inst-mo-count: one for a failed turn or a cancelled turn without text, two with a partial
    for (plan, removed) in [
        (turn("f").failed(), 1),
        (turn("c").cancelled(None), 1),
        (turn("p").cancelled(Some("part")), 2),
    ] {
        let rig = send_rig().await;
        let chat = rig.std_chat().await;
        let target = seed(&rig.quota.db, &chat, plan).await;
        let before = rig.message_count(chat.id).await;
        assert_eq!(before, removed);
        let reply = rig.delete_turn(chat.id, target.request_id).await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
        assert_eq!(rig.message_count(chat.id).await, 0);
    }
}

#[tokio::test]
async fn the_turn_status_of_a_deleted_turn_is_a_404_of_the_turn_type() {
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    rig.delete_turn(chat.id, second.request_id).await;
    let status = rig.get(&turn_uri(chat.id, second.request_id), &a1()).await;
    assert_not_found_of(&status, TURN_RESOURCE);
}

#[tokio::test]
async fn a_resend_of_the_deleted_request_id_is_a_409_request_id_conflict() {
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    rig.delete_turn(chat.id, second.request_id).await;
    let reply = rig
        .post_json(
            &stream_uri(chat.id),
            &a1(),
            &json!({ "content": "again", "request_id": second.request_id.to_string() }),
        )
        .await;
    let body = assert_problem(&reply, 409, "aborted");
    assert_eq!(body["context"]["reason"], "request_id_conflict");
}

#[tokio::test]
async fn the_previous_turn_becomes_the_latest_and_can_be_deleted_in_turn() {
    // inst-lt-skip-deleted: T2 deleted, then T1 is the latest
    let rig = send_rig().await;
    let (chat, first, second) = two_turns(&rig).await;
    assert_eq!(
        rig.delete_turn(chat.id, first.request_id).await.status,
        StatusCode::CONFLICT,
        "T1 is not the latest while T2 exists"
    );
    assert_eq!(
        rig.delete_turn(chat.id, second.request_id).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        rig.delete_turn(chat.id, first.request_id).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(rig.message_count(chat.id).await, 0);
    assert!(rig.list_messages(chat.id).await.is_empty());
}

#[tokio::test]
async fn a_delete_leaves_the_chat_activity_and_the_quota_rows_unchanged() {
    // inst-mc-delete-no-activity: the activity contract covers sent messages, retry and edit only
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    seed_rows(&rig.quota.db, &[row("daily", DAY, "total").spent(1_234)]).await;
    let quota_before = all_quota_rows(&rig.quota.db).await;
    let updated_before = chat_model(&rig.quota.db, chat.id).await.updated_at;
    rig.delete_turn(chat.id, second.request_id).await;
    assert_eq!(
        chat_model(&rig.quota.db, chat.id).await.updated_at,
        updated_before
    );
    assert_eq!(all_quota_rows(&rig.quota.db).await, quota_before);
    assert!(rig.events().await.is_empty(), "no usage event");
}

#[tokio::test]
async fn a_delete_runs_no_preflight_and_never_asks_the_policy() {
    // inst-del-commit: no quota preflight, no reserve and no provider call
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    rig.delete_turn(chat.id, second.request_id).await;
    assert_eq!(
        rig.gateway.total_calls(),
        0,
        "no policy snapshot, no limits"
    );
    assert_eq!(rig.provider.call_count(), 0);
}

#[tokio::test]
async fn a_delete_works_while_the_quota_is_exhausted_and_the_model_is_gone() {
    // no preflight: neither the quota nor the catalog can reject a delete
    let rig = send_rig().await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.quota.chat_a1("ghost-model").await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.delete_turn(chat.id, target.request_id).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
}

#[tokio::test]
async fn one_turn_delete_audit_event_is_enqueued_with_no_content() {
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    rig.delete_turn(chat.id, second.request_id).await;
    let events = mutation_events(&rig).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let AuditEvent::TurnDelete(delete) = &events[0] else {
        panic!("expected turn_delete, got {events:?}");
    };
    assert_eq!(delete.actor_user_id, USER_A1);
    assert_eq!(delete.chat_id, chat.id);
    assert_eq!(delete.request_id, second.request_id);
    let wire = serde_json::to_string(&events[0]).unwrap();
    assert!(!wire.contains("second"), "{wire}");
    assert_eq!(rig.audit_events().await.len(), 1);
}

#[tokio::test]
async fn a_delete_evaluates_the_pdp_once_with_the_delete_turn_action() {
    // inst-del-admission
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    rig.delete_turn(chat.id, second.request_id).await;
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "delete_turn");
    assert_eq!(request.resource.id, Some(chat.id));
}

// ── rejections ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_non_uuid_path_is_a_400_invalid_path_params() {
    // inst-del-bad-path, inst-del-return-bad-path
    let rig = send_rig().await;
    let (chat, _, _) = two_turns(&rig).await;
    let good = Uuid::new_v4();
    for uri in [
        format!("/mini-chat/v1/chats/nope/turns/{good}"),
        format!("/mini-chat/v1/chats/{}/turns/nope", chat.id),
    ] {
        let reply = rig.send(request("DELETE", &uri, &a1())).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        let violations = body["context"]["field_violations"].as_array().unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v["reason"] == "invalid_path_params"),
            "{uri}: {body}"
        );
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_pdp_deny_and_a_pdp_failure_leave_the_turn_in_place() {
    // inst-del-refused, inst-del-return-refusal
    for (pdp, status) in [(Pdp::Deny, 403), (Pdp::Unreachable, 503)] {
        let rig = send_rig_with(SendRigOptions::new().with_pdp(pdp)).await;
        let (chat, _, second) = two_turns(&rig).await;
        let before = snapshot(&rig.quota.db, &chat).await;
        let reply = rig.delete_turn(chat.id, second.request_id).await;
        if status == 403 {
            assert_authz_denied(&reply);
        } else {
            assert_problem(&reply, 503, "service_unavailable");
            assert_eq!(reply.retry_after.as_deref(), Some("5"));
        }
        assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
    }
}

#[tokio::test]
async fn a_chat_of_another_user_or_tenant_or_an_unknown_chat_is_a_404_of_the_chat_type() {
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    for (who, chat_id) in [(a1(), Uuid::new_v4()), (a2(), chat.id), (b1(), chat.id)] {
        let reply = rig.delete_turn_as(&who, chat_id, second.request_id).await;
        assert_chat_not_found(&reply);
    }
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn an_unknown_request_id_is_a_404_of_the_turn_type() {
    let rig = send_rig().await;
    let (chat, _, _) = two_turns(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.delete_turn(chat.id, Uuid::new_v4()).await;
    assert_not_found_of(&reply, TURN_RESOURCE);
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn a_turn_of_another_requester_is_a_403_authz_denied() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let foreign = seed(&rig.quota.db, &chat, turn("q").requester(Some(USER_A2))).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.delete_turn(chat.id, foreign.request_id).await;
    assert_authz_denied(&reply);
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn a_turn_that_is_not_the_latest_is_a_409_not_latest_turn() {
    let rig = send_rig().await;
    let (chat, first, _) = two_turns(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.delete_turn(chat.id, first.request_id).await;
    assert_conflict(&reply, "NOT_LATEST_TURN");
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn a_turn_behind_a_newer_running_turn_is_a_409_not_latest_turn() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    seed(&rig.quota.db, &chat, turn("second").at(100).running()).await;
    let reply = rig.delete_turn(chat.id, first.request_id).await;
    assert_conflict(&reply, "NOT_LATEST_TURN");
}

#[tokio::test]
async fn an_already_deleted_turn_is_a_409_not_latest_turn() {
    // inst-tv-final: Deleted stays Deleted
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    assert_eq!(
        rig.delete_turn(chat.id, second.request_id).await.status,
        StatusCode::NO_CONTENT
    );
    let before = snapshot(&rig.quota.db, &chat).await;
    let again = rig.delete_turn(chat.id, second.request_id).await;
    assert_conflict(&again, "NOT_LATEST_TURN");
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn an_already_replaced_turn_cannot_be_deleted() {
    // inst-tv-final: Replaced stays Replaced
    let (rig, chat, _upstream) = crate::test_support::mutations::streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = crate::test_support::mutations::started_ids(
        &crate::test_support::mutations::sse_of(&reply),
    );
    rig.wait_turn_terminal(chat.id, new_request).await;
    let again = rig.delete_turn(chat.id, target.request_id).await;
    assert_conflict(&again, "NOT_LATEST_TURN");
}

#[tokio::test]
async fn a_running_turn_is_a_400_turn_state_precondition() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let running = seed(&rig.quota.db, &chat, turn("q").running()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.delete_turn(chat.id, running.request_id).await;
    assert_state_violation(&reply, "turn_state");
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn a_forced_audit_payload_overflow_is_a_500_and_nothing_changes() {
    // inst-mc-payload-too-large, inst-mc-return-payload-500, inst-del-return-commit
    let outbox = FailingMutationOutbox::payload_too_large();
    let rig = send_rig_with(SendRigOptions::new().with_turn_outbox(outbox.clone())).await;
    outbox.bind(rig.quota.enqueuer.clone());
    let (chat, _, second) = two_turns(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.delete_turn(chat.id, second.request_id).await;
    assert_internal(&reply);
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
    assert_eq!(live_messages(&rig.quota.db, chat.id).await.len(), 4);
}

#[tokio::test]
async fn a_forced_database_error_is_a_generic_500_and_nothing_changes() {
    // inst-mc-db-error, inst-mc-return-500
    let rig = send_rig().await;
    let (chat, _, second) = two_turns(&rig).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    crate::test_support::chats::fail_outbox_inserts(&rig.quota.db).await;
    let reply = rig.delete_turn(chat.id, second.request_id).await;
    assert_internal(&reply);
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}
