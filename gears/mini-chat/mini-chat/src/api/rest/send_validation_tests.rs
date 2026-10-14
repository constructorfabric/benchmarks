//! `POST /v1/chats/{id}/messages:stream` request validation over the real router: the body
//! extractor errors (415, 400, 422), `EMPTY_CONTENT`, the attachment shape, the defaults, and the
//! rule that validation runs before the PEP call and before the chat is loaded.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-request-validation`, flow `send-message` (steps 1 to
//! 3), algorithm `send-error-contract`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-send-preflight`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::Pdp;
use crate::test_support::chats::{
    a1, assert_chat_not_found, assert_invalid_argument, assert_problem, request_with_body,
    violation,
};
use crate::test_support::send::{
    SendRig, SendRigOptions, footprint, send_rig, send_rig_with, stream_uri,
};

async fn post(rig: &SendRig, chat_id: Uuid, body: &Value) -> crate::test_support::chats::Reply {
    rig.stream(chat_id, body).await
}

fn ids(n: usize) -> Vec<String> {
    (0..n).map(|_| Uuid::new_v4().to_string()).collect()
}

// ── the body extractor errors ────────────────────────────────────────────────

#[tokio::test]
async fn a_body_without_a_json_content_type_is_a_415_missing_json_content_type() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let reply = rig
            .send(request_with_body(
                "POST",
                &stream_uri(chat.id),
                &a1(),
                content_type,
                r#"{"content":"hi"}"#,
            ))
            .await;
        assert_invalid_argument(&reply, 415, "body", "missing_json_content_type");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn malformed_json_is_a_400_json_syntax_error() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for body in ["{", r#"{"content": }"#, "not json", r#"{"content":"x""#] {
        let reply = rig
            .send(request_with_body(
                "POST",
                &stream_uri(chat.id),
                &a1(),
                Some("application/json"),
                body,
            ))
            .await;
        assert_invalid_argument(&reply, 400, "body", "json_syntax_error");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_body_that_does_not_match_the_schema_is_a_422_invalid_json_body() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let bad = [
        json!({}),
        json!({ "request_id": Uuid::new_v4().to_string() }),
        json!({ "content": 5 }),
        json!({ "content": null }),
        json!({ "content": ["x"] }),
        json!({ "content": "hi", "request_id": "not-a-uuid" }),
        json!({ "content": "hi", "request_id": 7 }),
        json!({ "content": "hi", "attachment_ids": ["nope"] }),
        json!({ "content": "hi", "attachment_ids": "x" }),
        json!({ "content": "hi", "attachment_ids": [1, 2] }),
        json!({ "content": "hi", "web_search": {} }),
        json!({ "content": "hi", "web_search": { "enabled": "yes" } }),
        json!({ "content": "hi", "web_search": true }),
        json!([]),
        json!("just a string"),
    ];
    for body in bad {
        let reply = post(&rig, chat.id, &body).await;
        let problem = assert_invalid_argument(&reply, 422, "body", "invalid_json_body");
        assert_eq!(
            violation(&problem, "body")["reason"],
            "invalid_json_body",
            "{body}"
        );
    }
    assert!(rig.pdp.requests().is_empty());
    assert_eq!(footprint(&rig.quota.db, &chat).await.turns, 0);
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_a_400_invalid_path_params_before_the_body() {
    let rig = send_rig().await;
    let reply = rig
        .post_json(
            "/mini-chat/v1/chats/nope/messages:stream",
            &a1(),
            &json!({ "content": "" }),
        )
        .await;
    let body = assert_problem(&reply, 400, "invalid_argument");
    let violations = body["context"]["field_violations"].as_array().unwrap();
    assert!(
        violations
            .iter()
            .any(|v| v["reason"] == "invalid_path_params"),
        "{body}"
    );
    assert!(rig.pdp.requests().is_empty());
}

// ── content ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn empty_and_whitespace_only_content_is_a_400_empty_content() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for content in ["", " ", "   ", "\n", "\t \r\n"] {
        let reply = post(&rig, chat.id, &json!({ "content": content })).await;
        let problem = assert_invalid_argument(&reply, 400, "content", "EMPTY_CONTENT");
        assert_eq!(violation(&problem, "content")["reason"], "EMPTY_CONTENT");
    }
    assert!(rig.pdp.requests().is_empty());
    assert_eq!(footprint(&rig.quota.db, &chat).await.messages, 0);
}

// ── attachment shape ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_duplicate_attachment_id_is_a_400_invalid_attachment() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let id = Uuid::new_v4().to_string();
    let reply = post(
        &rig,
        chat.id,
        &json!({ "content": "hi", "attachment_ids": [id, Uuid::new_v4().to_string(), id] }),
    )
    .await;
    let problem = assert_invalid_argument(&reply, 400, "attachment", "invalid_attachment");
    assert_eq!(
        violation(&problem, "attachment")["reason"],
        "invalid_attachment"
    );
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn more_attachment_ids_than_documents_plus_images_is_a_400_invalid_attachment() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let reply = post(
        &rig,
        chat.id,
        &json!({ "content": "hi", "attachment_ids": ids(55) }),
    )
    .await;
    assert_invalid_argument(&reply, 400, "attachment", "invalid_attachment");
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn the_attachment_cap_is_inclusive() {
    // 54 ids pass the shape check; the chat does not exist, so the request ends in a 404 (which
    // proves validation passed and the chat was looked up afterwards)
    let rig = send_rig().await;
    let reply = post(
        &rig,
        Uuid::new_v4(),
        &json!({ "content": "hi", "attachment_ids": ids(54) }),
    )
    .await;
    assert_chat_not_found(&reply);
}

// ── validation comes first ───────────────────────────────────────────────────

#[tokio::test]
async fn validation_runs_before_the_pdp_call_and_the_chat_load() {
    let rig = send_rig_with(SendRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = rig.std_chat().await;
    // a denied PDP and an unknown chat would both answer 403 or 404 for a valid body
    let denied = post(&rig, chat.id, &json!({ "content": "  " })).await;
    assert_invalid_argument(&denied, 400, "content", "EMPTY_CONTENT");
    let unknown = post(&rig, Uuid::new_v4(), &json!({ "content": "" })).await;
    assert_invalid_argument(&unknown, 400, "content", "EMPTY_CONTENT");
    let id = Uuid::new_v4().to_string();
    let dup = post(
        &rig,
        Uuid::new_v4(),
        &json!({ "content": "hi", "attachment_ids": [id, id] }),
    )
    .await;
    assert_invalid_argument(&dup, 400, "attachment", "invalid_attachment");
    assert!(rig.pdp.requests().is_empty(), "the PEP was never asked");
}

#[tokio::test]
async fn empty_content_is_reported_before_a_bad_attachment_list() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let id = Uuid::new_v4().to_string();
    let reply = post(
        &rig,
        chat.id,
        &json!({ "content": " ", "attachment_ids": [id, id] }),
    )
    .await;
    assert_invalid_argument(&reply, 400, "content", "EMPTY_CONTENT");
}

// ── defaults and unknown fields ──────────────────────────────────────────────

#[tokio::test]
async fn unknown_body_fields_and_null_optionals_pass_validation() {
    let rig = send_rig().await;
    let unknown_chat = Uuid::new_v4();
    for body in [
        json!({ "content": "hi", "future_field": { "a": 1 } }),
        json!({ "content": "hi", "request_id": null }),
        json!({ "content": "hi", "web_search": null }),
        json!({ "content": "hi", "attachment_ids": [] }),
        json!({ "content": "hi", "request_id": Uuid::nil().to_string() }),
        json!({ "content": "hi", "web_search": { "enabled": false, "extra": 1 } }),
    ] {
        // the unknown chat answers 404 only if validation passed
        let reply = post(&rig, unknown_chat, &body).await;
        assert_chat_not_found(&reply);
    }
}
