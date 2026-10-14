//! `GET /v1/chats/{id}` over the real router, SQLite and the mock PDP: the chat detail shape,
//! `message_count`, model immutability on reads, admission and the 404 masking.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: flow
//! `cpt-cf-mini-chat-flow-chat-management-get-chat`; algorithms `message-count`,
//! `chat-detail-projection`, `model-immutability` (reads), `activity-ordering` (reads);
//! definitions of done `cpt-cf-mini-chat-dod-chat-management-get` and
//! `cpt-cf-mini-chat-dod-chat-management-model-immutability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use serde_json::Value;
use uuid::Uuid;

use crate::test_support::Pdp;
use crate::test_support::chats::{
    CHATS, a1, assert_chat_not_found, assert_problem, at, chat_row, chat_uri, detail_keys, keys,
    parse_ts, rig, seed_message, soft_delete_message,
};

#[tokio::test]
async fn get_returns_the_chat_detail_of_the_owner() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("Q3 plan"), "gpt-standard").await;
    let reply = rig.get(&chat_uri(chat.id), &a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    let body = reply.json();
    assert_eq!(keys(&body), detail_keys(true));
    assert_eq!(body["id"], chat.id.to_string());
    assert_eq!(body["model"], "gpt-standard");
    assert_eq!(body["title"], "Q3 plan");
    assert_eq!(body["is_temporary"], false);
    assert_eq!(body["message_count"], 0);
    assert_eq!(parse_ts(&body["created_at"]), at(0));
    assert_eq!(parse_ts(&body["updated_at"]), at(0));
}

#[tokio::test]
async fn an_untitled_chat_omits_the_title_instead_of_sending_null() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), None, "gpt-standard").await;
    let reply = rig.get(&chat_uri(chat.id), &a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = reply.json();
    assert_eq!(keys(&body), detail_keys(false));
    assert!(!reply.text.contains("\"title\""), "{}", reply.text);
}

#[tokio::test]
async fn the_body_never_carries_internal_fields_or_embedded_messages() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("t"), "gpt-standard").await;
    seed_message(&rig.db, &chat, "user").await;
    let reply = rig.get(&chat_uri(chat.id), &a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    for forbidden in [
        "user_id",
        "tenant_id",
        "deleted_at",
        "messages",
        "provider",
        "vector_store",
    ] {
        assert!(
            !reply.text.contains(forbidden),
            "`{forbidden}` in {}",
            reply.text
        );
    }
}

// ── message count ────────────────────────────────────────────────────────────

#[tokio::test]
async fn message_count_is_the_number_of_non_deleted_messages_of_all_roles() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("counted"), "gpt-standard").await;
    let other = rig.seed(&a1(), Some("other"), "gpt-standard").await;
    for role in ["user", "assistant", "user"] {
        seed_message(&rig.db, &chat, role).await;
    }
    let deleted = seed_message(&rig.db, &chat, "assistant").await;
    soft_delete_message(&rig.db, deleted).await;
    seed_message(&rig.db, &other, "user").await;

    let body = rig.get(&chat_uri(chat.id), &a1()).await.json();
    assert_eq!(body["message_count"], 3);
    let other_body = rig.get(&chat_uri(other.id), &a1()).await.json();
    assert_eq!(other_body["message_count"], 1);
}

#[tokio::test]
async fn messages_written_later_are_counted_without_any_change() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("growing"), "gpt-standard").await;
    for expected in 1..=3 {
        seed_message(&rig.db, &chat, "user").await;
        let body = rig.get(&chat_uri(chat.id), &a1()).await.json();
        assert_eq!(body["message_count"], expected);
    }
}

// ── reads are not activity; the stored model is served ───────────────────────

#[tokio::test]
async fn reading_a_chat_does_not_change_updated_at() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("quiet"), "gpt-standard").await;
    for _ in 0..2 {
        let reply = rig.get(&chat_uri(chat.id), &a1()).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    assert_eq!(chat_row(&rig.db, chat.id).await.unwrap().updated_at, at(0));
}

#[tokio::test]
async fn a_chat_whose_model_was_disabled_or_removed_is_still_served_with_its_stored_model() {
    let rig = rig(Pdp::TenantOnly).await;
    // `gpt-hidden` is disabled in the catalog, `retired` is not in the catalog at all
    for model in ["gpt-hidden", "retired"] {
        let chat = rig.seed(&a1(), Some("old"), model).await;
        let reply = rig.get(&chat_uri(chat.id), &a1()).await;
        assert_eq!(reply.status, StatusCode::OK, "{model}: {}", reply.text);
        assert_eq!(reply.json()["model"], model);
    }
    assert_eq!(rig.gateway.call_count(), 0, "get consults no catalog");
}

// ── admission ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_is_authorized_as_read_with_the_chat_id_and_required_constraints() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("t"), "gpt-standard").await;
    let reply = rig.get(&chat_uri(chat.id), &a1()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.id, Some(chat.id));
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_or_an_unusable_decision_is_a_403_even_for_an_own_or_missing_chat() {
    for behavior in [Pdp::Deny, Pdp::NoConstraints, Pdp::UnknownProperty] {
        let rig = rig(behavior).await;
        let chat = rig.seed(&a1(), Some("t"), "gpt-standard").await;
        for id in [chat.id, Uuid::new_v4()] {
            let reply = rig.get(&chat_uri(id), &a1()).await;
            let body = assert_problem(&reply, 403, "permission_denied");
            assert_eq!(body["context"]["reason"], "AUTHZ_DENIED", "{behavior:?}");
        }
    }
}

#[tokio::test]
async fn a_pdp_failure_is_a_503_with_retry_after_5() {
    let rig = rig(Pdp::Unreachable).await;
    let chat = rig.seed(&a1(), Some("t"), "gpt-standard").await;
    let reply = rig.get(&chat_uri(chat.id), &a1()).await;
    let body = assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    assert_eq!(body["context"]["retry_after_seconds"], 5);
}

// ── errors ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_chat_is_a_404_not_found_of_the_chat_resource_type() {
    let rig = rig(Pdp::TenantOnly).await;
    let id = Uuid::new_v4();
    let reply = rig.get(&chat_uri(id), &a1()).await;
    let body = assert_chat_not_found(&reply);
    assert_eq!(body["instance"], chat_uri(id));
    assert_eq!(body["trace_id"], "req-42");
}

#[tokio::test]
async fn a_non_uuid_path_id_is_a_400_invalid_path_params_before_authorization() {
    let rig = rig(Pdp::TenantOnly).await;
    for id in ["not-a-uuid", "123", "00000000-0000-0000-0000-00000000000g"] {
        let reply = rig.get(&format!("{CHATS}/{id}"), &a1()).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        let violations = body["context"]["field_violations"].as_array().unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v["reason"] == "invalid_path_params"),
            "{id}: {body}"
        );
    }
    assert!(
        rig.pdp.requests().is_empty(),
        "the path extractor runs first"
    );
}

#[tokio::test]
async fn the_not_found_of_a_missing_and_a_soft_deleted_chat_are_indistinguishable() {
    let rig = rig(Pdp::TenantOnly).await;
    let deleted = rig.seed(&a1(), Some("gone"), "gpt-standard").await;
    crate::test_support::raw_exec(
        &rig.db.raw,
        &format!(
            "UPDATE chats SET deleted_at = '2026-01-01 00:00:00+00:00' WHERE id = {}",
            crate::test_support::chats::blob(deleted.id)
        ),
    )
    .await
    .unwrap();
    let mut bodies: Vec<Value> = Vec::new();
    for id in [deleted.id, Uuid::new_v4()] {
        let reply = rig.get(&chat_uri(id), &a1()).await;
        let mut body = assert_chat_not_found(&reply);
        violation_free(&mut body);
        bodies.push(body);
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// Drop the parts of a Problem that echo the requested id.
fn violation_free(body: &mut Value) {
    let object = body.as_object_mut().unwrap();
    object.remove("instance");
    body["context"]
        .as_object_mut()
        .unwrap()
        .remove("resource_name");
}
