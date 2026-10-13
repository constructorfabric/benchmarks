#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1, end to end: the preview matrix of the three routes through the
//! router over the real stores (eligibility, ownership, the path and the
//! gates) and the turn delete with its 204, its soft-deletes, the summary
//! invalidation, the audit event and the metrics.

use axum::http::StatusCode;
use mini_chat_sdk::TurnMutationAuditEvent;
use serde_json::json;
use uuid::Uuid;

use crate::chat_test_env::{CHAT_TYPE, Reply};
use crate::domain::service::mutations::types::{MutationOp, MutationResult};
use crate::mutation_test_env::{MutationEnv, SeedTurn, TURN_TYPE};
use crate::stream_test_env::{Options, oa};
use crate::test_doubles::{
    OTHER_USER, PdpReply, USER, USER_TENANT, other_tenant_ctx, other_user_ctx, user_ctx,
};

// ---- assertions -------------------------------------------------------------------------------------

fn assert_conflict(reply: &Reply, reason: &str) {
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert!(reply.category().contains("aborted"), "{}", reply.text);
    assert_eq!(reply.json["context"]["reason"], reason, "{}", reply.text);
    assert_eq!(reply.resource_type(), TURN_TYPE, "{}", reply.text);
}

fn assert_not_terminal(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        reply.category().contains("failed_precondition"),
        "{}",
        reply.text
    );
    let violation = &reply.json["context"]["violations"][0];
    assert_eq!(violation["subject"], "turn_state", "{}", reply.text);
    assert_eq!(violation["type"], "STATE", "{}", reply.text);
}

fn assert_forbidden(reply: &Reply) {
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(
        reply.json["context"]["reason"], "AUTHZ_DENIED",
        "{}",
        reply.text
    );
}

fn is_problem(reply: &Reply) -> bool {
    reply
        .header("content-type")
        .unwrap_or_default()
        .contains("application/problem+json")
}

/// The three operations, by name, against `request` of the default chat.
async fn each_op(env: &MutationEnv, request: Uuid) -> Vec<(&'static str, Reply)> {
    vec![
        ("retry", env.retry(request).await),
        ("edit", env.edit(request, "a new text").await),
        ("delete", env.delete(request).await),
    ]
}

/// A refused mutation: a problem (never a stream), nothing changed anywhere,
/// the provider never reached.
async fn assert_refused_cleanly(env: &MutationEnv, before: &str, name: &str, reply: &Reply) {
    assert_ne!(reply.status, StatusCode::OK, "{name}: {}", reply.text);
    assert_ne!(reply.status, StatusCode::NO_CONTENT, "{name}");
    assert!(
        is_problem(reply),
        "{name}: a problem, not a stream: {:?}",
        reply.headers
    );
    assert_eq!(
        env.world().await,
        before,
        "{name}: chat_turns, messages, links, summaries, chat, quota and outbox unchanged"
    );
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "{name}: the provider heard nothing"
    );
}

// ==== eligibility ===================================================================================

#[tokio::test]
async fn a_turn_that_is_not_the_latest_is_not_latest_turn_for_each_operation() {
    let env = MutationEnv::new().await;
    let a = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.seed_turn(&SeedTurn::completed(2, 20)).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, a.request_id).await {
        assert_conflict(&reply, "NOT_LATEST_TURN");
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
}

#[tokio::test]
async fn while_the_latest_turn_runs_it_is_not_terminal_and_the_earlier_one_is_not_the_latest() {
    let env = MutationEnv::new().await;
    let a = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let b = env.seed_turn(&SeedTurn::running(2, 20)).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, b.request_id).await {
        assert_not_terminal(&reply);
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
    for (name, reply) in each_op(&env, a.request_id).await {
        assert_conflict(&reply, "NOT_LATEST_TURN");
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
}

#[tokio::test]
async fn a_turn_whose_stream_is_held_open_by_the_provider_is_not_terminal() {
    let env = MutationEnv::new().await;
    let a = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let feed = env.gate();
    let request = Uuid::from_u128(0xE2FF_0001);
    let mut held = env
        .open_as(
            &user_ctx(),
            env.chat,
            &json!({ "content": "second", "request_id": request }),
        )
        .await;
    held.until("stream_started").await;

    for (name, reply) in each_op(&env, request).await {
        assert_not_terminal(&reply);
        assert!(is_problem(&reply), "{name}");
    }
    for (_, reply) in each_op(&env, a.request_id).await {
        assert_conflict(&reply, "NOT_LATEST_TURN");
    }

    let (name, data) = oa::completed(1, 1);
    feed.send_event(name, &data);
    feed.end();
    held.rest().await;
}

#[tokio::test]
async fn once_the_latest_turn_is_terminal_each_operation_is_accepted() {
    for state in ["completed", "failed", "cancelled"] {
        for op in ["retry", "edit", "delete"] {
            let env = MutationEnv::new().await;
            let mut seed = SeedTurn::completed(1, 10);
            seed.state = match state {
                "failed" => "failed",
                "cancelled" => "cancelled",
                _ => "completed",
            };
            let seeded = env.seed_turn(&seed).await;
            env.script(&[oa::delta("ok"), oa::completed(10, 5)]);

            let reply = match op {
                "retry" => env.retry(seeded.request_id).await,
                "edit" => env.edit(seeded.request_id, "edited").await,
                _ => env.delete(seeded.request_id).await,
            };

            let expected = if op == "delete" {
                StatusCode::NO_CONTENT
            } else {
                StatusCode::OK
            };
            assert_eq!(reply.status, expected, "{state} {op}: {}", reply.text);
        }
    }
}

#[tokio::test]
async fn a_replaced_or_deleted_turn_is_not_the_latest_for_each_operation() {
    let env = MutationEnv::new().await;
    let deleted = env.seed_turn(&SeedTurn::completed(1, 10).deleted()).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, deleted.request_id).await {
        assert_conflict(&reply, "NOT_LATEST_TURN");
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
}

#[tokio::test]
async fn an_unknown_request_id_is_a_404_of_the_turn_type_for_each_operation() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, Uuid::from_u128(0xDEAD)).await {
        reply.assert_not_found(TURN_TYPE);
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
}

#[tokio::test]
async fn a_turn_of_another_chat_is_a_404_of_the_turn_type() {
    let env = MutationEnv::new().await;
    let other = env.seed_chat(2, "p").await;
    let foreign = env.seed_turn_in(other, &SeedTurn::completed(1, 10)).await;

    for (_, reply) in each_op(&env, foreign.request_id).await {
        reply.assert_not_found(TURN_TYPE);
    }
}

#[tokio::test]
async fn a_request_id_that_is_not_a_uuid_is_400_invalid_path_params() {
    let env = MutationEnv::new().await;
    let ctx = user_ctx();
    let before = env.world().await;

    for (method, suffix, body) in [
        ("POST", "not-a-uuid/retry", None),
        (
            "PATCH",
            "not-a-uuid",
            Some(("application/json", json!({ "content": "x" }).to_string())),
        ),
        ("DELETE", "not-a-uuid", None),
    ] {
        let reply = env
            .call(
                method,
                &format!("/mini-chat/v1/chats/{}/turns/{suffix}", env.chat),
                &ctx,
                body,
            )
            .await;

        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{method}: {}",
            reply.text
        );
        assert_eq!(reply.first_violation().1, "invalid_path_params", "{method}");
    }
    assert_eq!(env.world().await, before);
}

#[tokio::test]
async fn a_chat_id_that_is_not_a_uuid_is_400_invalid_path_params() {
    let env = MutationEnv::new().await;

    let reply = env
        .call(
            "DELETE",
            &format!(
                "/mini-chat/v1/chats/not-a-uuid/turns/{}",
                Uuid::from_u128(1)
            ),
            &user_ctx(),
            None,
        )
        .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.first_violation().1, "invalid_path_params");
}

#[tokio::test]
async fn a_missing_foreign_or_other_tenant_chat_is_a_404_of_the_chat_type() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    let missing = env
        .call(
            "DELETE",
            &MutationEnv::item_uri(Uuid::from_u128(0xDEAD), seeded.request_id),
            &user_ctx(),
            None,
        )
        .await;
    let other_user = env
        .call(
            "DELETE",
            &MutationEnv::item_uri(env.chat, seeded.request_id),
            &other_user_ctx(),
            None,
        )
        .await;
    let other_tenant = env
        .call(
            "POST",
            &MutationEnv::retry_uri(env.chat, seeded.request_id),
            &other_tenant_ctx(),
            None,
        )
        .await;

    missing.assert_not_found(CHAT_TYPE);
    other_user.assert_not_found(CHAT_TYPE);
    other_tenant.assert_not_found(CHAT_TYPE);
    assert_eq!(env.world().await, before);
}

#[tokio::test]
async fn a_chat_that_is_soft_deleted_is_a_404_of_the_chat_type() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.quota.db.soft_delete("chats", env.chat).await;

    for (_, reply) in each_op(&env, seeded.request_id).await {
        reply.assert_not_found(CHAT_TYPE);
    }
}

#[tokio::test]
async fn a_turn_of_another_requester_is_403_authz_denied_for_each_operation() {
    let env = MutationEnv::new().await;
    let foreign = env
        .seed_turn(&SeedTurn::completed(1, 10).requester(Some(OTHER_USER)))
        .await;
    let system = env
        .seed_turn(&SeedTurn::completed(2, 5).requester(None))
        .await;
    let before = env.world().await;

    for request in [foreign.request_id, system.request_id] {
        for (name, reply) in each_op(&env, request).await {
            assert_forbidden(&reply);
            assert_refused_cleanly(&env, &before, name, &reply).await;
        }
    }
}

#[tokio::test]
async fn with_equal_start_times_only_the_turn_with_the_greater_id_is_mutable() {
    let env = MutationEnv::new().await;
    let lesser = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let greater = env.seed_turn(&SeedTurn::completed(2, 10)).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, lesser.request_id).await {
        assert_conflict(&reply, "NOT_LATEST_TURN");
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
    let accepted = env.delete(greater.request_id).await;

    assert_eq!(accepted.status, StatusCode::NO_CONTENT, "{}", accepted.text);
}

// ==== authorization ================================================================================

#[tokio::test]
async fn each_operation_is_authorized_once_with_its_own_action_on_the_chat() {
    for (op, action) in [
        ("retry", "retry_turn"),
        ("edit", "edit_turn"),
        ("delete", "delete_turn"),
    ] {
        let env = MutationEnv::new().await;
        let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
        env.script(&[oa::completed(1, 1)]);

        match op {
            "retry" => env.retry(seeded.request_id).await,
            "edit" => env.edit(seeded.request_id, "edited").await,
            _ => env.delete(seeded.request_id).await,
        };

        let requests = env.quota.pdp.requests();
        assert_eq!(
            requests.len(),
            1,
            "{op}: the preflight and the stream add no evaluation"
        );
        assert_eq!(requests[0].action.name, action);
        assert_eq!(requests[0].resource.id, Some(env.chat));
    }
}

#[tokio::test]
async fn a_pdp_deny_is_403_and_changes_nothing_for_each_operation() {
    let env = MutationEnv::with(Options {
        pdp: PdpReply::Deny,
        ..Options::default()
    })
    .await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, seeded.request_id).await {
        assert_forbidden(&reply);
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
}

#[tokio::test]
async fn a_pdp_failure_is_503_with_retry_after_5_and_changes_nothing() {
    let env = MutationEnv::with(Options {
        pdp: PdpReply::Failure,
        ..Options::default()
    })
    .await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    for (name, reply) in each_op(&env, seeded.request_id).await {
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{name}: {}",
            reply.text
        );
        assert_eq!(reply.header("retry-after"), Some("5"), "{name}");
        assert_refused_cleanly(&env, &before, name, &reply).await;
    }
}

#[tokio::test]
async fn another_users_chat_is_404_also_when_the_pdp_returns_only_a_tenant_predicate() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    let reply = env
        .call(
            "DELETE",
            &MutationEnv::item_uri(env.chat, seeded.request_id),
            &other_user_ctx(),
            None,
        )
        .await;

    reply.assert_not_found(CHAT_TYPE);
}

#[tokio::test]
async fn a_pdp_that_scopes_by_the_chat_itself_does_not_break_the_child_table_queries() {
    // The `id` term of this PDP names the chat: it must never constrain the
    // turn, message, link or summary statements (whose own `id` is another id).
    for op in ["retry", "edit", "delete"] {
        let env = MutationEnv::with(Options {
            pdp: PdpReply::AllowChatConstraints,
            ..Options::default()
        })
        .await;
        let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;
        env.script(&[oa::completed(1, 1)]);

        let reply = match op {
            "retry" => env.retry(seeded.request_id).await,
            "edit" => env.edit(seeded.request_id, "edited").await,
            _ => env.delete(seeded.request_id).await,
        };

        let expected = if op == "delete" {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::OK
        };
        assert_eq!(reply.status, expected, "{op}: {}", reply.text);
    }
}

// ==== the turn delete ================================================================================

#[tokio::test]
async fn a_delete_answers_204_with_an_empty_body() {
    let env = MutationEnv::new().await;
    let seeded = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    let reply = env.delete(seeded.request_id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(reply.text.is_empty());
}

#[tokio::test]
async fn a_deleted_turn_and_its_messages_leave_the_list_the_count_and_the_status() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let old = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    let detail_uri = format!("/mini-chat/v1/chats/{}", env.chat);
    let count_before = env.get(&detail_uri).await.json["message_count"]
        .as_u64()
        .unwrap();

    let reply = env.delete(old.request_id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(
        env.get(&detail_uri).await.json["message_count"]
            .as_u64()
            .unwrap(),
        count_before - 2
    );
    let list = env
        .get(&format!("/mini-chat/v1/chats/{}/messages", env.chat))
        .await;
    let contents: Vec<String> = list.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(contents, ["question 1", "answer 1"]);
    env.turn_status(old.request_id)
        .await
        .assert_not_found(TURN_TYPE);
}

#[tokio::test]
async fn deleting_a_failed_turn_removes_one_message() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::failed(1, 10)).await;
    let detail_uri = format!("/mini-chat/v1/chats/{}", env.chat);
    let before = env.get(&detail_uri).await.json["message_count"]
        .as_u64()
        .unwrap();

    env.delete(old.request_id).await;

    assert_eq!(
        env.get(&detail_uri).await.json["message_count"]
            .as_u64()
            .unwrap(),
        before - 1
    );
}

#[tokio::test]
async fn the_previous_turn_becomes_mutable_after_a_delete() {
    let env = MutationEnv::new().await;
    let previous = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let latest = env.seed_turn(&SeedTurn::completed(2, 20)).await;

    assert_eq!(
        env.delete(latest.request_id).await.status,
        StatusCode::NO_CONTENT
    );
    let second = env.delete(previous.request_id).await;

    assert_eq!(second.status, StatusCode::NO_CONTENT, "{}", second.text);
}

#[tokio::test]
async fn deleting_again_is_not_the_latest_turn_and_adds_no_event() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    env.delete(old.request_id).await;
    let after_first = env.world().await;

    let again = env.delete(old.request_id).await;

    assert_conflict(&again, "NOT_LATEST_TURN");
    assert_eq!(env.world().await, after_first);
    assert_eq!(env.mutation_events().await.len(), 1);
}

#[tokio::test]
async fn a_delete_bumps_no_updated_at_and_touches_no_link_reaction_or_quota() {
    let env = MutationEnv::new().await;
    let doc = env.seed_attachment(env.chat, 1, |row| row).await;
    let old = env
        .seed_turn(&SeedTurn::completed(1, 10).attached(&[doc]))
        .await;
    let (updated, links, quota) = (
        env.chat_updated_at().await,
        env.quota.dump("message_attachments").await,
        env.quota.dump("quota_usage").await,
    );

    let reply = env.delete(old.request_id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(env.turn_deleted_at(old.request_id).await.is_some());
    assert_eq!(env.chat_updated_at().await, updated);
    assert_eq!(env.quota.dump("message_attachments").await, links);
    assert_eq!(env.quota.dump("quota_usage").await, quota);
    assert!(env.usage_events().await.is_empty());
}

#[tokio::test]
async fn a_delete_enqueues_exactly_one_turn_delete_event_without_content() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;

    env.delete(old.request_id).await;

    let events = env.mutation_events().await;
    assert_eq!(events.len(), 1);
    match &events[0] {
        TurnMutationAuditEvent::TurnDelete {
            tenant_id,
            actor_user_id,
            chat_id,
            request_id,
            ..
        } => {
            assert_eq!(*tenant_id, USER_TENANT);
            assert_eq!(*actor_user_id, USER);
            assert_eq!(*chat_id, env.chat);
            assert_eq!(*request_id, old.request_id);
        }
        other => panic!("a turn_delete event expected, got {other:?}"),
    }
    let raw = serde_json::to_string(&env.audit_payloads().await).unwrap();
    assert!(!raw.contains("question 1") && !raw.contains("answer 1"));
}

#[tokio::test]
async fn a_delete_of_a_turn_covered_by_the_summary_invalidates_it() {
    let env = MutationEnv::new().await;
    env.seed_turn(&SeedTurn::completed(1, 10).compressed())
        .await;
    let old = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    env.seed_summary_at(old.assistant_message, 21).await;

    env.delete(old.request_id).await;

    assert_eq!(env.summary_count().await, 0);
    assert_eq!(env.compressed_count().await, 0);
}

#[tokio::test]
async fn a_summary_before_the_deleted_turn_is_kept() {
    let env = MutationEnv::new().await;
    let previous = env
        .seed_turn(&SeedTurn::completed(1, 10).compressed())
        .await;
    let old = env.seed_turn(&SeedTurn::completed(2, 20)).await;
    env.seed_summary_at(previous.assistant_message, 11).await;

    let reply = env.delete(old.request_id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(env.summary_count().await, 1);
    assert_eq!(env.compressed_count().await, 2);
}

#[tokio::test]
async fn a_failing_delete_transaction_is_a_500_and_nothing_is_left_behind() {
    let healthy = MutationEnv::new().await;
    let fine = healthy.seed_turn(&SeedTurn::completed(1, 10)).await;
    assert_eq!(
        healthy.delete(fine.request_id).await.status,
        StatusCode::NO_CONTENT
    );
    let env = MutationEnv::with_failing_mutation_audit().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let before = env.world().await;

    let reply = env.delete(old.request_id).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(env.world().await, before);
    assert!(
        !reply.text.contains("not_registered"),
        "no internal detail: {}",
        reply.text
    );
}

#[tokio::test]
async fn a_delete_and_its_conflicts_record_their_metric_once_per_request() {
    let env = MutationEnv::new().await;
    let a = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let b = env.seed_turn(&SeedTurn::completed(2, 20)).await;

    env.delete(a.request_id).await; // not the latest
    env.delete(b.request_id).await; // ok
    env.delete(b.request_id).await; // already deleted
    env.delete(Uuid::from_u128(0xDEAD)).await; // unknown

    assert_eq!(
        env.mutation_metrics.results(),
        vec![
            (MutationOp::Delete, MutationResult::NotLatestTurn),
            (MutationOp::Delete, MutationResult::Ok),
            (MutationOp::Delete, MutationResult::NotLatestTurn),
            (MutationOp::Delete, MutationResult::Rejected),
        ]
    );
    assert_eq!(
        env.mutation_metrics.latencies().len(),
        1,
        "one committed delete"
    );
}

#[tokio::test]
async fn the_delete_route_runs_no_quota_step_and_opens_no_stream() {
    let env = MutationEnv::new().await;
    let old = env.seed_turn(&SeedTurn::completed(1, 10)).await;
    let quota = env.quota.dump("quota_usage").await;

    let reply = env.delete(old.request_id).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.quota.dump("quota_usage").await, quota);
    assert!(
        env.quota.metrics.events().is_empty(),
        "no preflight, no reserve metric"
    );
}
