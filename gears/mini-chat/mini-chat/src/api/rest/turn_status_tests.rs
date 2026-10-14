//! `GET /v1/chats/{id}/turns/{request_id}` over the real router, SQLite and the mock PDP: the
//! state mapping, `error_code` and `assistant_message_id` rules, the omitted fields, the absence
//! of internal data and the 404 masking.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: flow
//! `cpt-cf-mini-chat-flow-message-streaming-turn-status`; algorithms `turn-status` and
//! `send-error-contract`; definition of done `cpt-cf-mini-chat-dod-message-streaming-turn-status`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use uuid::Uuid;

use crate::test_support::Pdp;
use crate::test_support::chats::{
    a1, a2, assert_chat_not_found, assert_problem, b1, chat_row, keys, parse_ts,
};
use crate::test_support::quota::STANDARD;
use crate::test_support::send::{
    SendRig, SendRigOptions, TURN_RESOURCE, TurnSeedFull, footprint, seed_turn_full, send_rig,
    send_rig_with, t, turn_uri,
};

async fn status(
    rig: &SendRig,
    chat_id: Uuid,
    request_id: Uuid,
) -> crate::test_support::chats::Reply {
    rig.get(&turn_uri(chat_id, request_id), &a1()).await
}

fn assert_turn_not_found(reply: &crate::test_support::chats::Reply) -> serde_json::Value {
    let body = assert_problem(reply, 404, "not_found");
    assert_eq!(body["context"]["resource_type"], TURN_RESOURCE, "{body}");
    body
}

#[tokio::test]
async fn a_running_turn_reports_running_with_request_id_and_updated_at_only() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("running").updated(t(42));
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    let reply = status(&rig, chat.id, request_id).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    let body = reply.json();
    assert_eq!(
        keys(&body),
        BTreeSet::from(["request_id", "state", "updated_at"])
    );
    assert_eq!(body["request_id"], request_id.to_string());
    assert_eq!(body["state"], "running");
    assert_eq!(parse_ts(&body["updated_at"]), t(42));
}

#[tokio::test]
async fn a_completed_turn_is_done_with_its_assistant_message_and_no_error_code() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let assistant = Uuid::new_v4();
    let seed = TurnSeedFull::new("completed")
        .assistant(assistant)
        .model(STANDARD);
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    let body = status(&rig, chat.id, request_id).await.json();
    assert_eq!(
        keys(&body),
        BTreeSet::from(["request_id", "state", "updated_at", "assistant_message_id"])
    );
    assert_eq!(body["state"], "done");
    assert_eq!(body["assistant_message_id"], assistant.to_string());
}

#[tokio::test]
async fn a_failed_turn_is_error_with_its_error_code_and_no_assistant_message() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for code in [
        "provider_error",
        "orphan_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "turn_setup_failed",
    ] {
        let seed = TurnSeedFull::new("failed").error(code);
        let request_id = seed.request_id;
        seed_turn_full(&rig.quota.db, &chat, seed).await;
        let body = status(&rig, chat.id, request_id).await.json();
        assert_eq!(
            keys(&body),
            BTreeSet::from(["request_id", "state", "updated_at", "error_code"]),
            "{code}"
        );
        assert_eq!(body["state"], "error");
        assert_eq!(body["error_code"], code);
    }
}

#[tokio::test]
async fn a_cancelled_turn_reports_its_partial_message_only_when_one_was_persisted() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let partial = Uuid::new_v4();
    let with = TurnSeedFull::new("cancelled").assistant(partial);
    let without = TurnSeedFull::new("cancelled");
    let (with_id, without_id) = (with.request_id, without.request_id);
    seed_turn_full(&rig.quota.db, &chat, with).await;
    seed_turn_full(&rig.quota.db, &chat, without).await;

    let body = status(&rig, chat.id, with_id).await.json();
    assert_eq!(body["state"], "cancelled");
    assert_eq!(body["assistant_message_id"], partial.to_string());
    assert!(body.get("error_code").is_none());

    let body = status(&rig, chat.id, without_id).await.json();
    assert_eq!(body["state"], "cancelled");
    assert_eq!(
        keys(&body),
        BTreeSet::from(["request_id", "state", "updated_at"])
    );
}

#[tokio::test]
async fn absent_optional_fields_are_omitted_never_null_and_internals_are_never_exposed() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("failed")
        .error("provider_error")
        .model(STANDARD);
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    let reply = status(&rig, chat.id, request_id).await;
    assert!(!reply.text.contains("null"), "{}", reply.text);
    for forbidden in [
        "chat_id",
        "tenant_id",
        "requester",
        "billing",
        "settlement",
        "reserve",
        "credits",
        "provider_response_id",
        "effective_model",
        "error_detail",
        "started_at",
        "deleted_at",
        "policy_version",
    ] {
        assert!(
            !reply.text.contains(forbidden),
            "`{forbidden}` in {}",
            reply.text
        );
    }
}

#[tokio::test]
async fn an_unknown_or_soft_deleted_turn_is_a_404_for_the_turn_type() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("completed")
        .assistant(Uuid::new_v4())
        .deleted(t(9));
    let deleted = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    for request_id in [Uuid::new_v4(), deleted] {
        assert_turn_not_found(&status(&rig, chat.id, request_id).await);
    }
}

#[tokio::test]
async fn a_turn_of_another_chat_is_not_found_in_this_one() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let other = rig.std_chat().await;
    let seed = TurnSeedFull::new("completed").assistant(Uuid::new_v4());
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &other, seed).await;
    assert_turn_not_found(&status(&rig, chat.id, request_id).await);
}

#[tokio::test]
async fn a_chat_that_is_not_visible_is_a_404_for_the_chat_type_whatever_the_turn() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("completed").assistant(Uuid::new_v4());
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    let foreign_user = rig.quota.chat(&a2(), STANDARD).await;
    for (who, id) in [
        (a2(), chat.id),
        (b1(), chat.id),
        (a1(), foreign_user.id),
        (a1(), Uuid::new_v4()),
    ] {
        let reply = rig.get(&turn_uri(id, request_id), &who).await;
        assert_chat_not_found(&reply);
    }
}

#[tokio::test]
async fn a_non_uuid_chat_id_or_request_id_is_a_400_invalid_path_params() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let good = Uuid::new_v4();
    for uri in [
        format!("/mini-chat/v1/chats/nope/turns/{good}"),
        format!("/mini-chat/v1/chats/{}/turns/nope", chat.id),
        format!("/mini-chat/v1/chats/{}/turns/123", chat.id),
    ] {
        let reply = rig.get(&uri, &a1()).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        let violations = body["context"]["field_violations"].as_array().unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v["reason"] == "invalid_path_params"),
            "{uri}: {body}"
        );
    }
    assert!(
        rig.pdp.requests().is_empty(),
        "the path extractor runs first"
    );
}

#[tokio::test]
async fn admission_evaluates_read_turn_with_the_chat_id_as_the_resource() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("running");
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    assert_eq!(
        status(&rig, chat.id, request_id).await.status,
        StatusCode::OK
    );
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "read_turn");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_denied_or_failing_pdp_refuses_the_status_read() {
    let denied = send_rig_with(SendRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = denied.std_chat().await;
    let reply = status(&denied, chat.id, Uuid::new_v4()).await;
    let body = assert_problem(&reply, 403, "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");

    let down = send_rig_with(SendRigOptions::new().with_pdp(Pdp::Unreachable)).await;
    let chat = down.std_chat().await;
    let reply = status(&down, chat.id, Uuid::new_v4()).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
}

#[tokio::test]
async fn the_read_changes_nothing() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("running").updated(t(3));
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let turn_before = crate::test_support::send::turn_rows(&rig.quota.db, chat.id).await;
    for _ in 0..3 {
        assert_eq!(
            status(&rig, chat.id, request_id).await.status,
            StatusCode::OK
        );
    }
    assert_eq!(footprint(&rig.quota.db, &chat).await, before);
    assert_eq!(
        crate::test_support::send::turn_rows(&rig.quota.db, chat.id).await,
        turn_before
    );
    assert_eq!(
        chat_row(&rig.quota.db, chat.id).await.unwrap().updated_at,
        crate::test_support::chats::at(0)
    );
}
