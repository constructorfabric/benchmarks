//! `PUT` and `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` over the real router and SQLite:
//! the 200 body (`message_id`, `reaction`, `created_at`), the `my_reaction` projection of the
//! messages list, the idempotent upsert (one row per user and message, a switch replaces the
//! value), the 204 of a delete with and without a reaction, assistant messages only (400
//! `reaction_target` / `STATE` for user and system messages on both methods), the 404 masking of
//! missing, foreign and soft-deleted chats and messages, the body and path validation before
//! authorization (415, `json_syntax_error`, 422, `INVALID_REACTION`, `invalid_path_params`), the
//! single PDP evaluation per request, `tenant_id` and `user_id` from the security context, and
//! the absence of side effects.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flows
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction` (`inst-setr-request`,
//! `inst-setr-validate`, `inst-setr-invalid`, `inst-setr-return-invalid`, `inst-setr-admission`,
//! `inst-setr-refused`, `inst-setr-return-refusal`, `inst-setr-target`, `inst-setr-upsert`,
//! `inst-setr-return-200`) and `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction`
//! (`inst-delr-request`, `inst-delr-validate`, `inst-delr-admission`, `inst-delr-refused`,
//! `inst-delr-return-refusal`, `inst-delr-target`, `inst-delr-remove`, `inst-delr-return-204`);
//! algorithms `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation`
//! (`inst-rv-path`, `inst-rv-return-path`, `inst-rv-delete`, `inst-rv-return-delete`,
//! `inst-rv-content-type`, `inst-rv-return-415`, `inst-rv-json-syntax`, `inst-rv-return-syntax`,
//! `inst-rv-schema`, `inst-rv-return-422`, `inst-rv-value`, `inst-rv-return-value`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target` (`inst-rt-select`,
//! `inst-rt-missing`, `inst-rt-return-404`, `inst-rt-not-assistant`, `inst-rt-return-400`,
//! `inst-rt-any-assistant`), `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert`
//! and `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal`; state
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-reaction`; definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-reactions`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::chats::{
    a1, a2, assert_chat_not_found, assert_invalid_argument, assert_problem, b1, keys,
    outbox_body_count, parse_ts, request, request_with_body, soft_delete_message,
};
use crate::test_support::mutations::{
    MESSAGE_RESOURCE, SeededTurn, assert_authz_denied, assert_internal, assert_not_found_of,
    assert_state_violation, reaction_rows, reaction_uri, seed, streaming_chat, turn,
};
use crate::test_support::send::{
    MessageSeed, SendRig, SendRigOptions, chat_model, seed_msg, seed_reaction_of, send_rig,
    send_rig_with, t,
};
use crate::test_support::{Pdp, TENANT_A, USER_A1, USER_A2};

async fn fixture() -> (
    SendRig,
    crate::infra::storage::repo_impl::ChatRecord,
    SeededTurn,
) {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("question").answer("answer")).await;
    (rig, chat, seeded)
}

async fn my_reaction(rig: &SendRig, chat_id: Uuid, message_id: Uuid) -> Value {
    rig.list_messages(chat_id)
        .await
        .into_iter()
        .find(|item| item["id"] == message_id.to_string())
        .unwrap_or_else(|| panic!("message {message_id} is not listed"))["my_reaction"]
        .clone()
}

// ── PUT ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_put_is_a_200_with_the_message_id_the_reaction_and_created_at() {
    // inst-setr-return-200
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    let reply = rig.put_reaction(chat.id, assistant, "like").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    let body = reply.json();
    assert_eq!(
        keys(&body),
        BTreeSet::from(["message_id", "reaction", "created_at"])
    );
    assert_eq!(body["message_id"], assistant.to_string());
    assert_eq!(body["reaction"], "like");
    let stored = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(parse_ts(&body["created_at"]), stored[0].created_at);
}

#[tokio::test]
async fn the_next_messages_list_shows_the_value_in_my_reaction() {
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, Value::Null);
    rig.put_reaction(chat.id, assistant, "like").await;
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, "like");
    rig.put_reaction(chat.id, assistant, "dislike").await;
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, "dislike");
    // the user message of the same turn has none
    assert_eq!(
        my_reaction(&rig, chat.id, seeded.user_message_id).await,
        Value::Null
    );
}

#[tokio::test]
async fn a_repeated_put_and_a_switch_leave_exactly_one_row() {
    // inst-rx-same, inst-rx-like-dislike, inst-rx-dislike-like, inst-ru-one-row
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for value in ["like", "like", "dislike", "dislike", "like"] {
        let reply = rig.put_reaction(chat.id, assistant, value).await;
        assert_eq!(reply.status, StatusCode::OK, "{value}: {}", reply.text);
        assert_eq!(reply.json()["reaction"], value);
        let rows = reaction_rows(&rig.quota.db, assistant).await;
        assert_eq!(rows.len(), 1, "after {value}");
        assert_eq!(rows[0].reaction, value);
    }
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, "like");
}

#[tokio::test]
async fn the_row_takes_tenant_and_user_from_the_security_context_never_from_the_body() {
    // inst-ru-upsert
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    let reply = rig
        .put_reaction_as(
            &a1(),
            chat.id,
            assistant,
            &json!({ "reaction": "like", "user_id": USER_A2.to_string(), "tenant_id": Uuid::new_v4().to_string() }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, USER_A1);
    assert_eq!(rows[0].tenant_id, TENANT_A);
}

#[tokio::test]
async fn an_empty_completed_answer_and_a_partial_message_accept_a_reaction() {
    // inst-rt-any-assistant
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let empty = seed(&rig.quota.db, &chat, turn("q").answer("").at(10)).await;
    let partial = seed(
        &rig.quota.db,
        &chat,
        turn("q2").cancelled(Some("par")).at(100),
    )
    .await;
    for message in [
        empty.assistant_message_id.unwrap(),
        partial.assistant_message_id.unwrap(),
    ] {
        let reply = rig.put_reaction(chat.id, message, "dislike").await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
}

#[tokio::test]
async fn a_put_has_no_side_effect_on_the_chat_the_outbox_or_the_message() {
    // inst-ru-no-side-effects
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    let messages_before = crate::test_support::chats::message_rows(&rig.quota.db, chat.id).await;
    let updated_before = chat_model(&rig.quota.db, chat.id).await.updated_at;
    let outbox_before = outbox_body_count(&rig.quota.db).await;
    rig.put_reaction(chat.id, assistant, "like").await;
    rig.put_reaction(chat.id, assistant, "dislike").await;
    rig.delete_reaction(chat.id, assistant).await;
    assert_eq!(
        chat_model(&rig.quota.db, chat.id).await.updated_at,
        updated_before
    );
    assert_eq!(outbox_body_count(&rig.quota.db).await, outbox_before);
    assert_eq!(
        crate::test_support::chats::message_rows(&rig.quota.db, chat.id).await,
        messages_before
    );
    assert!(rig.audit_events().await.is_empty());
}

#[tokio::test]
async fn two_concurrent_puts_both_succeed_and_leave_one_row() {
    // inst-ru-one-row: no 409 and no 500 for the unique conflict
    for round in 0..5 {
        let (rig, chat, seeded) = fixture().await;
        let assistant = seeded.assistant_message_id.unwrap();
        let (a, b) = tokio::join!(
            rig.put_reaction(chat.id, assistant, "like"),
            rig.put_reaction(chat.id, assistant, "dislike"),
        );
        assert_eq!(a.status, StatusCode::OK, "round {round}: {}", a.text);
        assert_eq!(b.status, StatusCode::OK, "round {round}: {}", b.text);
        assert_eq!(reaction_rows(&rig.quota.db, assistant).await.len(), 1);
    }
}

// ── DELETE ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_delete_is_a_204_with_no_body_whether_or_not_a_reaction_existed() {
    // inst-delr-return-204, inst-rr-idempotent, inst-rx-remove, inst-rx-remove-none
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    let none = rig.delete_reaction(chat.id, assistant).await;
    assert_eq!(none.status, StatusCode::NO_CONTENT, "{}", none.text);
    assert!(none.text.is_empty());
    rig.put_reaction(chat.id, assistant, "like").await;
    let removed = rig.delete_reaction(chat.id, assistant).await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.text);
    assert!(removed.text.is_empty());
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, Value::Null);
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
    let again = rig.delete_reaction(chat.id, assistant).await;
    assert_eq!(again.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_delete_removes_only_the_reaction_of_the_caller() {
    // inst-rr-owner-only
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    seed_reaction_of(&rig.quota.db, TENANT_A, USER_A2, assistant, "like").await;
    rig.put_reaction(chat.id, assistant, "dislike").await;
    rig.delete_reaction(chat.id, assistant).await;
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, USER_A2);
}

#[tokio::test]
async fn the_reaction_of_another_user_is_not_in_my_reaction() {
    // the projection carries the caller's value only
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    seed_reaction_of(&rig.quota.db, TENANT_A, USER_A2, assistant, "dislike").await;
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, Value::Null);
    rig.put_reaction(chat.id, assistant, "like").await;
    assert_eq!(my_reaction(&rig, chat.id, assistant).await, "like");
    assert_eq!(reaction_rows(&rig.quota.db, assistant).await.len(), 2);
}

// ── the target ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_user_or_a_system_message_is_a_400_reaction_target_on_put_and_delete() {
    // inst-rt-not-assistant, inst-rt-return-400
    let (rig, chat, seeded) = fixture().await;
    let system = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("system", "sys", t(5)).request(None),
    )
    .await;
    for message in [seeded.user_message_id, system] {
        let put = rig.put_reaction(chat.id, message, "like").await;
        assert_state_violation(&put, "reaction_target");
        let delete = rig.delete_reaction(chat.id, message).await;
        assert_state_violation(&delete, "reaction_target");
    }
    assert!(
        reaction_rows(&rig.quota.db, seeded.user_message_id)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn an_unknown_message_or_a_message_of_another_chat_is_a_404_of_the_message_type() {
    // inst-rt-missing, inst-rt-return-404
    let (rig, chat, seeded) = fixture().await;
    let other = rig.std_chat().await;
    let foreign = seed(&rig.quota.db, &other, turn("elsewhere")).await;
    for message in [Uuid::new_v4(), foreign.assistant_message_id.unwrap()] {
        let put = rig.put_reaction(chat.id, message, "like").await;
        assert_not_found_of(&put, MESSAGE_RESOURCE);
        let delete = rig.delete_reaction(chat.id, message).await;
        assert_not_found_of(&delete, MESSAGE_RESOURCE);
    }
    assert!(
        reaction_rows(&rig.quota.db, foreign.assistant_message_id.unwrap())
            .await
            .is_empty()
    );
    assert_ne!(seeded.assistant_message_id, foreign.assistant_message_id);
}

#[tokio::test]
async fn a_soft_deleted_message_is_a_404_of_the_message_type() {
    // a message of a replaced or deleted turn
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    rig.put_reaction(chat.id, assistant, "like").await;
    soft_delete_message(&rig.quota.db, assistant).await;
    let put = rig.put_reaction(chat.id, assistant, "dislike").await;
    assert_not_found_of(&put, MESSAGE_RESOURCE);
    let delete = rig.delete_reaction(chat.id, assistant).await;
    assert_not_found_of(&delete, MESSAGE_RESOURCE);
    // the row of the deleted message is kept for audit and unchanged
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reaction, "like");
}

#[tokio::test]
async fn a_message_of_a_replaced_turn_is_not_a_target_and_the_new_answer_is() {
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q").answer("old")).await;
    let old_answer = target.assistant_message_id.unwrap();
    rig.put_reaction(chat.id, old_answer, "like").await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, message_id) = crate::test_support::mutations::started_ids(
        &crate::test_support::mutations::sse_of(&reply),
    );
    rig.wait_turn_terminal(chat.id, new_request).await;

    let put = rig.put_reaction(chat.id, old_answer, "dislike").await;
    assert_not_found_of(&put, MESSAGE_RESOURCE);
    assert_eq!(
        reaction_rows(&rig.quota.db, old_answer).await.len(),
        1,
        "kept for audit"
    );
    let fresh = rig.put_reaction(chat.id, message_id, "like").await;
    assert_eq!(fresh.status, StatusCode::OK, "{}", fresh.text);
    assert_eq!(my_reaction(&rig, chat.id, message_id).await, "like");
}

// ── admission and masking ────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_foreign_or_deleted_chat_is_a_404_of_the_chat_type() {
    // inst-setr-return-refusal, inst-delr-return-refusal
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for (who, chat_id) in [(a1(), Uuid::new_v4()), (a2(), chat.id), (b1(), chat.id)] {
        let put = rig
            .put_reaction_as(&who, chat_id, assistant, &json!({ "reaction": "like" }))
            .await;
        assert_chat_not_found(&put);
        let delete = rig.delete_reaction_as(&who, chat_id, assistant).await;
        assert_chat_not_found(&delete);
    }
    crate::test_support::mutations::soft_delete_chat(&rig.quota.db, chat.id).await;
    assert_chat_not_found(&rig.put_reaction(chat.id, assistant, "like").await);
    assert_chat_not_found(&rig.delete_reaction(chat.id, assistant).await);
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
}

#[tokio::test]
async fn a_pdp_deny_and_a_pdp_failure_are_403_and_503_with_retry_after_5() {
    // inst-setr-refused, inst-delr-refused
    for (pdp, status) in [(Pdp::Deny, 403), (Pdp::Unreachable, 503)] {
        let rig = send_rig_with(SendRigOptions::new().with_pdp(pdp)).await;
        let chat = rig.std_chat().await;
        let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
        let assistant = seeded.assistant_message_id.unwrap();
        let put = rig.put_reaction(chat.id, assistant, "like").await;
        let delete = rig.delete_reaction(chat.id, assistant).await;
        for reply in [&put, &delete] {
            if status == 403 {
                assert_authz_denied(reply);
            } else {
                assert_problem(reply, 503, "service_unavailable");
                assert_eq!(reply.retry_after.as_deref(), Some("5"));
            }
        }
        assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
    }
}

#[tokio::test]
async fn each_method_evaluates_the_pdp_once_with_its_own_action() {
    // inst-setr-admission, inst-delr-admission
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    rig.put_reaction(chat.id, assistant, "like").await;
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "set_reaction");
    assert_eq!(request.resource.id, Some(chat.id));

    let (rig, chat, seeded) = fixture().await;
    rig.delete_reaction(chat.id, seeded.assistant_message_id.unwrap())
        .await;
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "delete_reaction");
    assert_eq!(request.resource.id, Some(chat.id));
}

// ── the validation before authorization ──────────────────────────────────────

#[tokio::test]
async fn a_value_other_than_like_or_dislike_is_a_400_invalid_reaction_before_authorization() {
    // inst-rv-value, inst-rv-return-value
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for value in [
        "",
        "Like",
        "LIKE",
        " like",
        "love",
        "thumbs_up",
        "1",
        "null",
    ] {
        let reply = rig.put_reaction(chat.id, assistant, value).await;
        assert_invalid_argument(&reply, 400, "reaction", "INVALID_REACTION");
    }
    // before authorization: the same Problem for a chat that does not exist
    let reply = rig
        .put_reaction(Uuid::new_v4(), Uuid::new_v4(), "love")
        .await;
    assert_invalid_argument(&reply, 400, "reaction", "INVALID_REACTION");
    assert!(rig.pdp.requests().is_empty());
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
}

#[tokio::test]
async fn a_body_without_a_json_content_type_is_a_415_missing_json_content_type() {
    // inst-rv-content-type, inst-rv-return-415
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let reply = rig
            .put_reaction_raw(chat.id, assistant, content_type, r#"{"reaction":"like"}"#)
            .await;
        assert_invalid_argument(&reply, 415, "body", "missing_json_content_type");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn malformed_json_is_a_400_json_syntax_error() {
    // inst-rv-json-syntax, inst-rv-return-syntax
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for body in [
        "{",
        r#"{"reaction": }"#,
        "not json",
        r#"{"reaction":"like""#,
    ] {
        let reply = rig
            .put_reaction_raw(chat.id, assistant, Some("application/json"), body)
            .await;
        assert_invalid_argument(&reply, 400, "body", "json_syntax_error");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_body_that_does_not_match_the_schema_is_a_422_invalid_json_body() {
    // inst-rv-schema, inst-rv-return-422: no `reaction` field, or not a string
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for body in [
        json!({}),
        json!({ "kind": "like" }),
        json!({ "reaction": 1 }),
        json!({ "reaction": null }),
        json!({ "reaction": ["like"] }),
        json!([]),
        json!("like"),
    ] {
        let reply = rig.put_reaction_as(&a1(), chat.id, assistant, &body).await;
        assert_invalid_argument(&reply, 422, "body", "invalid_json_body");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_non_uuid_chat_or_message_id_is_a_400_invalid_path_params() {
    // inst-rv-path, inst-rv-return-path, inst-delr-validate
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    for uri in [
        format!("/mini-chat/v1/chats/nope/messages/{assistant}/reaction"),
        format!("/mini-chat/v1/chats/{}/messages/nope/reaction", chat.id),
        format!("/mini-chat/v1/chats/{}/messages/123/reaction", chat.id),
    ] {
        for method in ["PUT", "DELETE"] {
            let reply = if method == "PUT" {
                rig.send(request_with_body(
                    method,
                    &uri,
                    &a1(),
                    Some("application/json"),
                    r#"{"reaction":"like"}"#,
                ))
                .await
            } else {
                rig.send(request(method, &uri, &a1())).await
            };
            let body = assert_problem(&reply, 400, "invalid_argument");
            let violations = body["context"]["field_violations"].as_array().unwrap();
            assert!(
                violations
                    .iter()
                    .any(|v| v["reason"] == "invalid_path_params"),
                "{method} {uri}: {body}"
            );
        }
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_delete_needs_no_body_and_ignores_the_validation_of_a_put() {
    // inst-rv-delete, inst-rv-return-delete
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    let reply = rig
        .send(request("DELETE", &reaction_uri(chat.id, assistant), &a1()))
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
}

#[tokio::test]
async fn a_database_error_on_the_upsert_or_the_removal_is_a_generic_500() {
    // inst-ru-db-error, inst-ru-return-500, inst-rr-db-error, inst-rr-return-500
    let (rig, chat, seeded) = fixture().await;
    let assistant = seeded.assistant_message_id.unwrap();
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        "CREATE TRIGGER fail_reaction_insert BEFORE INSERT ON message_reactions \
         BEGIN SELECT RAISE(ABORT, 'forced insert failure'); END",
    )
    .await
    .unwrap();
    let put = rig.put_reaction(chat.id, assistant, "like").await;
    assert_internal(&put);
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
    crate::test_support::raw_exec(&rig.quota.db.raw, "DROP TRIGGER fail_reaction_insert")
        .await
        .unwrap();

    assert_eq!(
        rig.put_reaction(chat.id, assistant, "like").await.status,
        StatusCode::OK
    );
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        "CREATE TRIGGER fail_reaction_delete BEFORE DELETE ON message_reactions \
         BEGIN SELECT RAISE(ABORT, 'forced delete failure'); END",
    )
    .await
    .unwrap();
    let delete = rig.delete_reaction(chat.id, assistant).await;
    assert_internal(&delete);
    assert_eq!(
        reaction_rows(&rig.quota.db, assistant).await.len(),
        1,
        "the row stays"
    );
}
