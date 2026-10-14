//! `PATCH /v1/chats/{id}` over the real router, SQLite and the mock PDP.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: flow
//! `cpt-cf-mini-chat-flow-chat-management-update-chat-title`; algorithms `title-validation`,
//! `model-immutability`, `activity-ordering` (rename), `message-count`; state `chat-lifecycle`
//! (Active to Active); definitions of done `cpt-cf-mini-chat-dod-chat-management-update` and
//! `cpt-cf-mini-chat-dod-chat-management-model-immutability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::test_support::Pdp;
use crate::test_support::chats::{
    CHATS, a1, assert_chat_not_found, assert_invalid_argument, assert_problem, at, attachment_rows,
    chat_row, chat_uri, detail_keys, item_ids, keys, message_rows, parse_ts, request_with_body,
    rig, seed_attachment, seed_message,
};

fn chars(c: char, n: usize) -> String {
    std::iter::repeat_n(c, n).collect()
}

async fn listed(rig: &crate::test_support::chats::ChatRig) -> Vec<Uuid> {
    item_ids(&rig.get(CHATS, &a1()).await.json())
}

fn title_body(title: &str) -> String {
    json!({ "title": title }).to_string()
}

#[tokio::test]
async fn patch_returns_200_with_the_updated_chat_detail() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("before"), "gpt-standard").await;
    seed_message(&rig.db, &chat, "user").await;
    seed_message(&rig.db, &chat, "assistant").await;
    let started = OffsetDateTime::now_utc();

    let reply = rig
        .patch_json(&chat_uri(chat.id), &a1(), &title_body("  Renamed  "))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = reply.json();
    assert_eq!(keys(&body), detail_keys(true));
    assert_eq!(body["id"], chat.id.to_string());
    assert_eq!(body["title"], "Renamed", "trimmed");
    assert_eq!(body["model"], "gpt-standard", "unchanged");
    assert_eq!(body["is_temporary"], false);
    assert_eq!(
        body["message_count"], 2,
        "a rename does not change the count"
    );
    assert_eq!(
        parse_ts(&body["created_at"]),
        at(0),
        "created_at is unchanged"
    );
    let updated = parse_ts(&body["updated_at"]);
    assert!(updated > at(0), "updated_at moves forward");
    assert!(
        updated >= started - time::Duration::seconds(1),
        "updated_at is the current time"
    );
}

#[tokio::test]
async fn the_response_is_the_row_as_written_and_a_later_get_agrees() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("before"), "gpt-standard").await;
    let patched = rig
        .patch_json(&chat_uri(chat.id), &a1(), &title_body("after"))
        .await;
    assert_eq!(patched.status, StatusCode::OK, "{}", patched.text);
    let patched = patched.json();

    let row = chat_row(&rig.db, chat.id).await.unwrap();
    assert_eq!(patched["title"], row.title.as_deref().unwrap());
    assert_eq!(
        parse_ts(&patched["updated_at"]),
        row.updated_at,
        "the written updated_at"
    );

    let read = rig.get(&chat_uri(chat.id), &a1()).await.json();
    assert_eq!(read, patched);
}

#[tokio::test]
async fn only_title_and_updated_at_are_written() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("before"), "gpt-standard").await;
    seed_message(&rig.db, &chat, "user").await;
    seed_attachment(&rig.db, &chat, "document", Some("pending")).await;
    seed_attachment(&rig.db, &chat, "image", None).await;
    let before = chat_row(&rig.db, chat.id).await.unwrap();
    let messages = message_rows(&rig.db, chat.id).await;
    let attachments = attachment_rows(&rig.db, chat.id).await;

    let reply = rig
        .patch_json(&chat_uri(chat.id), &a1(), &title_body("after"))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);

    let after = chat_row(&rig.db, chat.id).await.unwrap();
    assert_eq!(after.title.as_deref(), Some("after"));
    assert!(after.updated_at > before.updated_at);
    assert_eq!(after.model, before.model);
    assert_eq!(after.is_temporary, before.is_temporary);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.tenant_id, before.tenant_id);
    assert_eq!(after.user_id, before.user_id);
    assert_eq!(after.deleted_at, None);
    assert_eq!(message_rows(&rig.db, chat.id).await, messages);
    assert_eq!(attachment_rows(&rig.db, chat.id).await, attachments);
    assert!(
        rig.outbox_rows().await.is_empty(),
        "a rename enqueues nothing"
    );
}

#[tokio::test]
async fn unknown_body_fields_are_ignored_so_a_model_in_the_body_never_changes_the_model() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("before"), "gpt-standard").await;
    for extra in [
        json!({ "title": "Renamed", "model": "gpt-premium" }),
        json!({ "title": "Renamed", "model": "gpt-hidden", "is_temporary": true }),
        json!({ "title": "Renamed", "model": "never-existed", "user_id": Uuid::new_v4() }),
        json!({ "title": "Renamed", "model": null, "deleted_at": "2020-01-01T00:00:00Z" }),
    ] {
        let reply = rig
            .patch_json(&chat_uri(chat.id), &a1(), &extra.to_string())
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{extra}: {}", reply.text);
        let body = reply.json();
        assert_eq!(body["title"], "Renamed");
        assert_eq!(body["model"], "gpt-standard", "{extra}");
        assert_eq!(body["is_temporary"], false);
        let row = chat_row(&rig.db, chat.id).await.unwrap();
        assert_eq!(row.model, "gpt-standard");
        assert!(!row.is_temporary);
        assert_eq!(row.deleted_at, None);
    }
    assert_eq!(rig.gateway.call_count(), 0, "a rename consults no catalog");
}

#[tokio::test]
async fn a_chat_whose_model_left_the_catalog_can_still_be_renamed() {
    let rig = rig(Pdp::TenantOnly).await;
    for model in ["gpt-hidden", "retired"] {
        let chat = rig.seed(&a1(), Some("old"), model).await;
        let reply = rig
            .patch_json(&chat_uri(chat.id), &a1(), &title_body("still renamable"))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{model}: {}", reply.text);
        assert_eq!(reply.json()["model"], model);
    }
}

#[tokio::test]
async fn the_title_is_trimmed_and_accepted_from_1_to_255_characters() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("start"), "m").await;
    let at_limit = chars('t', 255);
    for (sent, stored) in [
        ("x".to_owned(), "x".to_owned()),
        (format!("\t {at_limit} \n"), at_limit.clone()),
        (chars('\u{00E9}', 255), chars('\u{00E9}', 255)),
    ] {
        let reply = rig
            .patch_json(&chat_uri(chat.id), &a1(), &title_body(&sent))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
        assert_eq!(reply.json()["title"], stored.as_str());
        assert_eq!(
            chat_row(&rig.db, chat.id).await.unwrap().title,
            Some(stored)
        );
    }
}

#[tokio::test]
async fn an_invalid_title_is_a_400_invalid_title_and_changes_nothing() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    for title in [
        String::new(),
        "   ".to_owned(),
        "\n\t".to_owned(),
        chars('a', 256),
    ] {
        let reply = rig
            .patch_json(&chat_uri(chat.id), &a1(), &title_body(&title))
            .await;
        assert_invalid_argument(&reply, 400, "title", "INVALID_TITLE");
    }
    let row = chat_row(&rig.db, chat.id).await.unwrap();
    assert_eq!(row.title.as_deref(), Some("keep"));
    assert_eq!(row.updated_at, at(0));
}

#[tokio::test]
async fn the_title_is_validated_before_the_chat_is_loaded_and_before_authorization() {
    // a chat that does not exist: the validation outcome does not depend on the chat
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig
        .patch_json(&chat_uri(Uuid::new_v4()), &a1(), &title_body("   "))
        .await;
    assert_invalid_argument(&reply, 400, "title", "INVALID_TITLE");
    assert!(rig.pdp.requests().is_empty());

    // a PDP that would deny
    let denying = crate::test_support::chats::rig(Pdp::Deny).await;
    let chat = denying.seed(&a1(), Some("t"), "m").await;
    let reply = denying
        .patch_json(&chat_uri(chat.id), &a1(), &title_body(""))
        .await;
    assert_invalid_argument(&reply, 400, "title", "INVALID_TITLE");
    assert!(denying.pdp.requests().is_empty());
}

// ── activity ordering ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_rename_sets_updated_at_again_even_to_the_same_title_and_orders_the_chat_first() {
    let rig = rig(Pdp::TenantOnly).await;
    let older = rig
        .seed_at(&a1(), Uuid::new_v4(), Some("older"), "m", at(10))
        .await;
    let newer = rig
        .seed_at(&a1(), Uuid::new_v4(), Some("newer"), "m", at(20))
        .await;
    assert_eq!(listed(&rig).await, vec![newer.id, older.id]);

    let first = rig
        .patch_json(&chat_uri(older.id), &a1(), &title_body("older"))
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    let first_updated = parse_ts(&first.json()["updated_at"]);
    assert!(
        first_updated > at(10),
        "the same title still bumps updated_at"
    );
    assert_eq!(listed(&rig).await, vec![older.id, newer.id]);

    let second = rig
        .patch_json(&chat_uri(older.id), &a1(), &title_body("older again"))
        .await;
    assert!(parse_ts(&second.json()["updated_at"]) > first_updated);
}

// ── admission ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn patch_is_authorized_as_update_with_the_chat_id_and_required_constraints() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("t"), "m").await;
    let reply = rig
        .patch_json(&chat_uri(chat.id), &a1(), &title_body("u"))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "update");
    assert_eq!(request.resource.id, Some(chat.id));
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn a_pdp_deny_or_failure_refuses_the_rename_and_changes_nothing() {
    for (behavior, status) in [
        (Pdp::Deny, 403),
        (Pdp::NoConstraints, 403),
        (Pdp::Unreachable, 503),
    ] {
        let rig = rig(behavior).await;
        let chat = rig.seed(&a1(), Some("keep"), "m").await;
        let reply = rig
            .patch_json(&chat_uri(chat.id), &a1(), &title_body("hijack"))
            .await;
        let body = assert_problem(
            &reply,
            status,
            if status == 403 {
                "permission_denied"
            } else {
                "service_unavailable"
            },
        );
        if status == 403 {
            assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
        } else {
            assert_eq!(reply.retry_after.as_deref(), Some("5"));
        }
        let row = chat_row(&rig.db, chat.id).await.unwrap();
        assert_eq!(row.title.as_deref(), Some("keep"), "{behavior:?}");
        assert_eq!(row.updated_at, at(0), "{behavior:?}");
    }
}

// ── errors ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_chat_is_a_404() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig
        .patch_json(&chat_uri(Uuid::new_v4()), &a1(), &title_body("x"))
        .await;
    assert_chat_not_found(&reply);
}

#[tokio::test]
async fn a_non_uuid_path_id_is_a_400_invalid_path_params() {
    let rig = rig(Pdp::TenantOnly).await;
    let reply = rig
        .patch_json(&format!("{CHATS}/not-a-uuid"), &a1(), &title_body("x"))
        .await;
    let body = assert_problem(&reply, 400, "invalid_argument");
    assert!(
        body["context"]["field_violations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["reason"] == "invalid_path_params"),
        "{body}"
    );
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_body_without_a_json_content_type_is_a_415_and_changes_nothing() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    for content_type in [None, Some("text/plain")] {
        let reply = rig
            .send(request_with_body(
                "PATCH",
                &chat_uri(chat.id),
                &a1(),
                content_type,
                &title_body("x"),
            ))
            .await;
        assert_invalid_argument(&reply, 415, "body", "missing_json_content_type");
    }
    assert_eq!(
        chat_row(&rig.db, chat.id).await.unwrap().title.as_deref(),
        Some("keep")
    );
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn malformed_json_is_a_400_json_syntax_error() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    for body in ["{", r#"{"title": }"#, "nope"] {
        let reply = rig.patch_json(&chat_uri(chat.id), &a1(), body).await;
        assert_invalid_argument(&reply, 400, "body", "json_syntax_error");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_missing_null_or_non_string_title_is_a_422_invalid_json_body() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    for body in [
        "{}",
        r#"{"title":null}"#,
        r#"{"title":5}"#,
        r#"{"title":true}"#,
        r#"{"title":["x"]}"#,
        r#"{"model":"gpt-standard"}"#,
        "[]",
    ] {
        let reply = rig.patch_json(&chat_uri(chat.id), &a1(), body).await;
        assert_invalid_argument(&reply, 422, "body", "invalid_json_body");
    }
    assert_eq!(
        chat_row(&rig.db, chat.id).await.unwrap().title.as_deref(),
        Some("keep")
    );
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_json_content_type_with_parameters_is_accepted() {
    let rig = rig(Pdp::TenantOnly).await;
    let chat = rig.seed(&a1(), Some("keep"), "m").await;
    let reply = rig
        .send(request_with_body(
            "PATCH",
            &chat_uri(chat.id),
            &a1(),
            Some("application/json; charset=utf-8"),
            &title_body("charset"),
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
}
