//! Message list contract and reaction tests through the router (DESIGN §3.3 "List Messages",
//! "Message Reaction API"). Turns are inserted directly through the entities.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use base64::Engine as _;
use http::StatusCode;
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue::Set, Condition};
use serde_json::{Value, json};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::clock;
use crate::infra::db::entities::{attachment, message, message_attachment, message_reaction};
use crate::testing::{PREMIUM, TENANT_A, TENANT_B, TestApp, USER_A1, USER_A2, USER_B1, ctx, ctx_a1};

const CHAT_RT: &str = "gts.cf.core.mini_chat.chat.v1~";
const MESSAGE_RT: &str = "gts.cf.core.mini_chat.message.v1~";
const ODATA_RT: &str = "gts.cf.core.odata.query.v1~";

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            _ => {
                write!(out, "%{b:02X}").unwrap();
            }
        }
    }
    out
}

fn messages_uri(chat: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages")
}

fn reaction_uri(chat: Uuid, msg: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages/{msg}/reaction")
}

struct Msg {
    role: &'static str,
    request_id: Option<Uuid>,
    model: Option<&'static str>,
    input_tokens: i64,
    output_tokens: i64,
    deleted: bool,
}

impl Msg {
    fn user(request_id: Uuid) -> Self {
        Self { role: "user", request_id: Some(request_id), model: None, input_tokens: 0, output_tokens: 0, deleted: false }
    }

    fn assistant(request_id: Uuid) -> Self {
        Self { role: "assistant", request_id: Some(request_id), model: Some(PREMIUM), input_tokens: 12, output_tokens: 7, deleted: false }
    }
}

async fn insert(t: &TestApp, chat_id: Uuid, m: Msg) -> Uuid {
    let now = clock::now();
    let am = message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        request_id: Set(m.request_id),
        role: Set(m.role.to_owned()),
        content: Set(format!("{} text", m.role)),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(m.model.map(|_| "resp_secret_provider_id".to_owned())),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(m.input_tokens),
        output_tokens: Set(m.output_tokens),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(m.model.map(str::to_owned)),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(m.deleted.then_some(now)),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<message::Entity>(am, &AccessScope::for_tenant(TENANT_A), &conn).await.unwrap().id
}

/// A completed turn: user + assistant message sharing a request id.
async fn turn(t: &TestApp, chat_id: Uuid) -> (Uuid, Uuid, Uuid) {
    let rid = Uuid::new_v4();
    let user = insert(t, chat_id, Msg::user(rid)).await;
    let assistant = insert(t, chat_id, Msg::assistant(rid)).await;
    (rid, user, assistant)
}

async fn insert_attachment(t: &TestApp, chat_id: Uuid, kind: &str, status: &str, thumb: bool, deleted: bool) -> Uuid {
    let now = clock::now();
    let am = attachment::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(USER_A1),
        filename: Set(format!("{kind}-{status}.bin")),
        content_type: Set(if kind == "image" { "image/png" } else { "application/pdf" }.to_owned()),
        size_bytes: Set(10),
        storage_backend: Set("openai".to_owned()),
        provider_file_id: Set(Some("file-secret".to_owned())),
        status: Set(status.to_owned()),
        error_code: Set(None),
        attachment_kind: Set(kind.to_owned()),
        for_file_search: Set(kind == "document"),
        for_code_interpreter: Set(false),
        doc_summary: Set(None),
        img_thumbnail: Set(thumb.then(|| vec![1, 2, 3, 4])),
        img_thumbnail_width: Set(thumb.then_some(64)),
        img_thumbnail_height: Set(thumb.then_some(32)),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(None),
        cleanup_attempts: Set(0),
        last_cleanup_error: Set(None),
        cleanup_updated_at: Set(None),
        created_at: Set(now),
        updated_at: Set(now),
        deleted_at: Set(deleted.then_some(now)),
        secondary_file_id: Set(None),
        secondary_status: Set("not_attempted".to_owned()),
        secondary_provider_kind: Set(None),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<attachment::Entity>(am, &AccessScope::for_tenant(TENANT_A), &conn).await.unwrap().id
}

async fn link(t: &TestApp, chat_id: Uuid, message_id: Uuid, attachment_id: Uuid) {
    let am = message_attachment::ActiveModel {
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        message_id: Set(message_id),
        attachment_id: Set(attachment_id),
        created_at: Set(clock::now()),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<message_attachment::Entity>(am, &AccessScope::for_tenant(TENANT_A), &conn).await.unwrap();
}

async fn react_directly(t: &TestApp, message_id: Uuid, user: Uuid, reaction: &str) {
    let am = message_reaction::ActiveModel {
        id: Set(Uuid::new_v4()),
        message_id: Set(message_id),
        user_id: Set(user),
        tenant_id: Set(TENANT_A),
        reaction: Set(reaction.to_owned()),
        created_at: Set(clock::now()),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<message_reaction::Entity>(am, &AccessScope::for_tenant(TENANT_A).ensure_owner(user), &conn)
        .await
        .unwrap();
}

async fn reaction_rows(t: &TestApp, message_id: Uuid) -> Vec<message_reaction::Model> {
    let conn = t.app.db.conn().unwrap();
    message_reaction::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(message_reaction::Column::MessageId.eq(message_id)))
        .all(&conn)
        .await
        .unwrap()
}

async fn list(t: &TestApp, chat: Uuid, query: &str) -> (StatusCode, Value) {
    let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{}{query}", messages_uri(chat)), None).await;
    (st, body)
}

fn item_ids(page: &Value) -> Vec<String> {
    page["items"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_owned()).collect()
}

// ───────────────────────────── list contract ─────────────────────────────

#[tokio::test]
async fn messages_contract_for_a_completed_turn() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (rid, user, assistant) = turn(&t, chat).await;
    let (st, page) = list(&t, chat, "").await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(item_ids(&page), vec![user.to_string(), assistant.to_string()]);
    assert_eq!(page["page_info"]["limit"], 20);
    let u = &page["items"][0];
    let a = &page["items"][1];
    assert_eq!(u["role"], "user");
    assert_eq!(u["request_id"], rid.to_string());
    assert_eq!(u["attachments"], json!([]));
    assert!(u["my_reaction"].is_null());
    assert!(u.as_object().unwrap().contains_key("my_reaction"), "my_reaction is always present");
    for absent in ["model", "input_tokens", "output_tokens"] {
        assert!(u.get(absent).is_none(), "user message must omit {absent}: {u}");
    }
    assert_eq!(u["content"], "user text");
    assert_eq!(a["role"], "assistant");
    assert_eq!(a["request_id"], rid.to_string());
    assert_eq!(a["model"], PREMIUM);
    assert_eq!(a["input_tokens"], 12);
    assert_eq!(a["output_tokens"], 7);
    assert_eq!(a["attachments"], json!([]));
    assert!(a.as_object().unwrap().contains_key("my_reaction") && a["my_reaction"].is_null());
    assert!(a["created_at"].is_string());
    let text = page.to_string();
    assert!(!text.contains("resp_secret_provider_id") && !text.contains("file-secret"), "provider ids leaked: {text}");
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "list_messages" && *c == Some(chat)));

    let (_, _, detail) = t.call(&ctx_a1(), "GET", &format!("/mini-chat/v1/chats/{chat}"), None).await;
    assert_eq!(detail["message_count"], 2);
}

#[tokio::test]
async fn zero_tokens_are_omitted_and_deleted_messages_excluded() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let rid = Uuid::new_v4();
    insert(&t, chat, Msg::user(rid)).await;
    let a = insert(&t, chat, Msg { input_tokens: 0, output_tokens: 0, ..Msg::assistant(rid) }).await;
    let rid2 = Uuid::new_v4();
    insert(&t, chat, Msg { deleted: true, ..Msg::user(rid2) }).await;
    insert(&t, chat, Msg { deleted: true, ..Msg::assistant(rid2) }).await;
    let (_, page) = list(&t, chat, "").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let item = &page["items"][1];
    assert_eq!(item["id"], a.to_string());
    assert!(item.get("input_tokens").is_none() && item.get("output_tokens").is_none(), "{item}");
    assert_eq!(item["model"], PREMIUM);
}

#[tokio::test]
async fn attachments_are_summarised_per_message() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (_, user, _) = turn(&t, chat).await;
    let (_, user2, _) = turn(&t, chat).await;
    let ready_img = insert_attachment(&t, chat, "image", "ready", true, false).await;
    let pending_img = insert_attachment(&t, chat, "image", "pending", true, false).await;
    let doc = insert_attachment(&t, chat, "document", "ready", false, false).await;
    let deleted = insert_attachment(&t, chat, "document", "ready", false, true).await;
    for a in [ready_img, pending_img, doc, deleted] {
        link(&t, chat, user, a).await;
    }
    link(&t, chat, user2, doc).await;

    let (st, page) = list(&t, chat, "").await;
    assert_eq!(st, StatusCode::OK, "{page}");
    let atts = page["items"][0]["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 3, "deleted attachment excluded: {atts:?}");
    let by_id = |id: Uuid| atts.iter().find(|a| a["attachment_id"] == id.to_string()).unwrap().clone();
    let img = by_id(ready_img);
    assert_eq!(img["kind"], "image");
    assert_eq!(img["status"], "ready");
    assert_eq!(img["filename"], "image-ready.bin");
    assert_eq!(img["img_thumbnail"]["content_type"], "image/webp");
    assert_eq!(img["img_thumbnail"]["width"], 64);
    assert_eq!(img["img_thumbnail"]["height"], 32);
    assert_eq!(
        img["img_thumbnail"]["data_base64"],
        base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 4])
    );
    let pending = by_id(pending_img);
    assert_eq!(pending["status"], "pending");
    assert!(pending.get("img_thumbnail").is_none(), "{pending}");
    let d = by_id(doc);
    assert_eq!(d["kind"], "document");
    assert!(d.get("img_thumbnail").is_none());
    assert!(by_id_opt(atts, deleted).is_none());
    // The assistant message has no rows; the second user message re-links the document.
    assert_eq!(page["items"][1]["attachments"], json!([]));
    let second = page["items"][2]["attachments"].as_array().unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["attachment_id"], doc.to_string());
}

fn by_id_opt(atts: &[Value], id: Uuid) -> Option<&Value> {
    atts.iter().find(|a| a["attachment_id"] == id.to_string())
}

#[tokio::test]
async fn my_reaction_is_per_user_and_null_for_user_messages() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (_, user, a1) = turn(&t, chat).await;
    let (_, _, a2) = turn(&t, chat).await;
    let (_, _, a3) = turn(&t, chat).await;
    react_directly(&t, a1, USER_A1, "like").await;
    react_directly(&t, a2, USER_A1, "dislike").await;
    react_directly(&t, a3, USER_A2, "like").await; // another user's reaction is not mine
    react_directly(&t, user, USER_A1, "like").await; // never exposed on a user message
    let (_, page) = list(&t, chat, "").await;
    let items = page["items"].as_array().unwrap();
    let reaction = |id: Uuid| items.iter().find(|m| m["id"] == id.to_string()).unwrap()["my_reaction"].clone();
    assert_eq!(reaction(a1), "like");
    assert_eq!(reaction(a2), "dislike");
    assert!(reaction(a3).is_null());
    assert!(reaction(user).is_null());
}

#[tokio::test]
async fn chronological_order_filters_and_pagination() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let mut all = Vec::new();
    let mut assistants = Vec::new();
    for _ in 0..4 {
        let (_, u, a) = turn(&t, chat).await;
        all.push(u.to_string());
        all.push(a.to_string());
        assistants.push(a.to_string());
    }
    let (_, page) = list(&t, chat, "").await;
    assert_eq!(item_ids(&page), all);

    let (st, page) = list(&t, chat, &format!("?$filter={}", enc("role eq 'assistant'"))).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(item_ids(&page), assistants);
    let (_, page) = list(&t, chat, &format!("?$filter={}", enc(&format!("id eq {}", assistants[1])))).await;
    assert_eq!(item_ids(&page), vec![assistants[1].clone()]);
    let (_, page) = list(&t, chat, &format!("?$orderby={}", enc("created_at desc"))).await;
    let mut rev = all.clone();
    rev.reverse();
    assert_eq!(item_ids(&page), rev);
    let (_, full) = list(&t, chat, "").await;
    let ts = full["items"][3]["created_at"].as_str().unwrap().to_owned();
    let (_, page) = list(&t, chat, &format!("?$filter={}", enc(&format!("created_at ge {ts}")))).await;
    assert_eq!(item_ids(&page), all[3..].to_vec());
    let (_, page) = list(&t, chat, &format!("?$filter={}", enc(&format!("created_at eq {ts}")))).await;
    assert_eq!(item_ids(&page), vec![all[3].clone()]);
    let (_, page) = list(&t, chat, "?$select=id,role").await;
    assert!(page["items"][0]["content"].is_string(), "$select is ignored");

    // Cursor pagination returns every message exactly once, in order.
    let mut seen = Vec::new();
    let (_, mut page) = list(&t, chat, "?limit=3").await;
    loop {
        seen.extend(item_ids(&page));
        let Some(c) = page["page_info"]["next_cursor"].as_str().map(str::to_owned) else { break };
        let (st, next) = list(&t, chat, &format!("?limit=3&cursor={}", enc(&c))).await;
        assert_eq!(st, StatusCode::OK, "{next}");
        page = next;
    }
    assert_eq!(seen, all);
}

#[tokio::test]
async fn list_errors() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    turn(&t, chat).await;
    for (query, expected) in [
        ("?limit=0".to_owned(), "INVALID_LIMIT"),
        (format!("?$filter={}", enc("content eq 'x'")), "INVALID_FILTER"),
        ("?cursor=garbage".to_owned(), "INVALID_CURSOR"),
    ] {
        let (st, body) = list(&t, chat, &query).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["context"]["resource_type"], ODATA_RT);
        assert_eq!(body["context"]["field_violations"][0]["reason"], expected, "{body}");
    }
    let (st, body) = list(&t, Uuid::new_v4(), "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["context"]["resource_type"], CHAT_RT);
    for other in [ctx(USER_A2, TENANT_A), ctx(USER_B1, TENANT_B)] {
        let (st, _, body) = t.call(&other, "GET", &messages_uri(chat), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    }
}

#[tokio::test]
async fn null_request_id_fails_with_internal_error() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    turn(&t, chat).await;
    insert(&t, chat, Msg { request_id: None, ..Msg::user(Uuid::nil()) }).await;
    let (st, body) = list(&t, chat, "").await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert!(body["type"].as_str().unwrap().contains("internal"));
    // A page that does not contain the row is served.
    let (st, _) = list(&t, chat, "?limit=2").await;
    assert_eq!(st, StatusCode::OK);
}

// ───────────────────────────── reactions ─────────────────────────────

#[tokio::test]
async fn put_reaction_upserts_one_row() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (_, _, a) = turn(&t, chat).await;
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(chat, a), Some(json!({"reaction": "like"}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["message_id"], a.to_string());
    assert_eq!(body["reaction"], "like");
    assert!(body["created_at"].is_string());
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(chat, a), Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["reaction"], "dislike");
    let rows = reaction_rows(&t, a).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(rows[0].user_id, USER_A1);
    assert_eq!(rows[0].tenant_id, TENANT_A);
    let (_, page) = list(&t, chat, "").await;
    assert_eq!(page["items"][1]["my_reaction"], "dislike");
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "set_reaction" && *c == Some(chat)));
}

#[tokio::test]
async fn invalid_reaction_value_is_checked_before_authorization() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (_, _, a) = turn(&t, chat).await;
    t.authz.deny.store(true, Ordering::SeqCst);
    t.authz.calls.lock().unwrap().clear();
    for bad in ["love", "LIKE", ""] {
        let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(chat, a), Some(json!({"reaction": bad}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["context"]["field_violations"][0]["field"], "reaction");
        assert_eq!(body["context"]["field_violations"][0]["reason"], "INVALID_REACTION");
    }
    assert!(t.authz.calls.lock().unwrap().is_empty());
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(chat, a), Some(json!({"reaction": "like"}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");
    t.authz.deny.store(false, Ordering::SeqCst);
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(chat, a), Some(json!({}))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn reaction_on_user_message_is_a_failed_precondition() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (_, u, _) = turn(&t, chat).await;
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(chat, u), Some(json!({"reaction": "like"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(body["type"].as_str().unwrap().contains("failed_precondition"));
    assert_eq!(body["context"]["violations"][0]["subject"], "reaction_target");
    assert_eq!(body["context"]["violations"][0]["type"], "STATE");
    let (st, _, body) = t.call(&ctx_a1(), "DELETE", &reaction_uri(chat, u), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["context"]["violations"][0]["subject"], "reaction_target");
    assert!(reaction_rows(&t, u).await.is_empty());
}

#[tokio::test]
async fn reaction_not_found_cases() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let other_chat = t.create_chat(&ctx_a1(), None).await;
    let (_, _, a) = turn(&t, chat).await;
    let (_, _, other_a) = turn(&t, other_chat).await;
    let rid = Uuid::new_v4();
    let deleted = insert(&t, chat, Msg { deleted: true, ..Msg::assistant(rid) }).await;
    for msg in [Uuid::new_v4(), other_a, deleted] {
        for (method, body) in [("PUT", Some(json!({"reaction": "like"}))), ("DELETE", None)] {
            let (st, _, body) = t.call(&ctx_a1(), method, &reaction_uri(chat, msg), body).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{method}: {body}");
            assert_eq!(body["context"]["resource_type"], MESSAGE_RT);
        }
    }
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &reaction_uri(Uuid::new_v4(), a), Some(json!({"reaction": "like"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["context"]["resource_type"], CHAT_RT);
    for other in [ctx(USER_A2, TENANT_A), ctx(USER_B1, TENANT_B)] {
        let (st, _, body) = t.call(&other, "PUT", &reaction_uri(chat, a), Some(json!({"reaction": "like"}))).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["context"]["resource_type"], CHAT_RT);
        let (st, _, _) = t.call(&other, "DELETE", &reaction_uri(chat, a), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
    let (st, _, body) = t.call(&ctx_a1(), "PUT", &format!("/mini-chat/v1/chats/{chat}/messages/xyz/reaction"), Some(json!({"reaction": "like"}))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert!(reaction_rows(&t, a).await.is_empty());
}

#[tokio::test]
async fn delete_reaction_is_idempotent() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (_, _, a) = turn(&t, chat).await;
    react_directly(&t, a, USER_A2, "dislike").await;
    t.call(&ctx_a1(), "PUT", &reaction_uri(chat, a), Some(json!({"reaction": "like"}))).await;
    for _ in 0..2 {
        let (st, _, body) = t.call(&ctx_a1(), "DELETE", &reaction_uri(chat, a), None).await;
        assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    }
    let rows = reaction_rows(&t, a).await;
    assert_eq!(rows.len(), 1, "only the caller's reaction is removed");
    assert_eq!(rows[0].user_id, USER_A2);
    let (_, page) = list(&t, chat, "").await;
    assert!(page["items"][1]["my_reaction"].is_null());
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "delete_reaction" && *c == Some(chat)));
}
