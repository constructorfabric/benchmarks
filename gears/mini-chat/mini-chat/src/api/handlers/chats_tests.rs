//! Chat CRUD / list / isolation tests through the router (DESIGN §3.3, §3.8, ADR-0004).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::atomic::Ordering;

use http::StatusCode;
use sea_orm::entity::prelude::*;
use sea_orm::{ActiveValue::Set, Condition};
use serde_json::{Value, json};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::clock;
use crate::domain::chats::touch_chat;
use crate::infra::db::entities::{attachment, chat, message};
use crate::infra::outbox::PAYLOAD_CHAT_CLEANUP;
use crate::testing::{PREMIUM, STANDARD, TENANT_A, TENANT_B, TestApp, USER_A1, USER_A2, USER_B1, ctx, ctx_a1};

const CHATS: &str = "/mini-chat/v1/chats";
const CHAT_RT: &str = "gts.cf.core.mini_chat.chat.v1~";
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

fn reason(body: &Value) -> &str {
    body["context"]["field_violations"][0]["reason"].as_str().unwrap_or_default()
}

async fn create(t: &TestApp, body: Value) -> (StatusCode, http::HeaderMap, Value) {
    t.call(&ctx_a1(), "POST", CHATS, Some(body)).await
}

async fn list(t: &TestApp, ctx: &toolkit_security::SecurityContext, query: &str) -> (StatusCode, Value) {
    let (st, _, body) = t.call(ctx, "GET", &format!("{CHATS}{query}"), None).await;
    (st, body)
}

fn ids(page: &Value) -> Vec<String> {
    page["items"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()).collect()
}

/// Inserts a message directly (the streaming service is not involved).
async fn insert_message(t: &TestApp, chat_id: Uuid, role: &str, deleted: bool) -> Uuid {
    let now = clock::now();
    let am = message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        request_id: Set(Some(Uuid::new_v4())),
        role: Set(role.to_owned()),
        content: Set("hi".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(now),
        deleted_at: Set(deleted.then_some(now)),
    };
    let conn = t.app.db.conn().unwrap();
    secure_insert::<message::Entity>(am, &AccessScope::for_tenant(TENANT_A), &conn).await.unwrap().id
}

async fn insert_attachment(t: &TestApp, chat_id: Uuid, deleted: bool, cleanup: Option<&str>) -> Uuid {
    let now = clock::now();
    let am = attachment::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        uploaded_by_user_id: Set(USER_A1),
        filename: Set("a.pdf".to_owned()),
        content_type: Set("application/pdf".to_owned()),
        size_bytes: Set(10),
        storage_backend: Set("openai".to_owned()),
        provider_file_id: Set(Some("file-1".to_owned())),
        status: Set("ready".to_owned()),
        error_code: Set(None),
        attachment_kind: Set("document".to_owned()),
        for_file_search: Set(true),
        for_code_interpreter: Set(false),
        doc_summary: Set(None),
        img_thumbnail: Set(None),
        img_thumbnail_width: Set(None),
        img_thumbnail_height: Set(None),
        summary_model: Set(None),
        summary_updated_at: Set(None),
        cleanup_status: Set(cleanup.map(str::to_owned)),
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

async fn attachment_row(t: &TestApp, id: Uuid) -> attachment::Model {
    let conn = t.app.db.conn().unwrap();
    attachment::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(attachment::Column::Id.eq(id)))
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

/// Minimal read-only view of the shared outbox body table (black-box check of the enqueue).
mod outbox_body {
    use sea_orm::entity::prelude::*;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "toolkit_outbox_body")]
    #[secure(unrestricted)]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub payload: Vec<u8>,
        pub payload_type: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

async fn chat_cleanup_payloads(t: &TestApp) -> Vec<Value> {
    let conn = t.app.db.conn().unwrap();
    outbox_body::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(outbox_body::Column::PayloadType.eq(PAYLOAD_CHAT_CLEANUP)))
        .all(&conn)
        .await
        .unwrap()
        .into_iter()
        .map(|b| serde_json::from_slice(&b.payload).unwrap())
        .collect()
}

// ───────────────────────────── create ─────────────────────────────

#[tokio::test]
async fn create_uses_default_model_and_sets_location() {
    let t = TestApp::new().await;
    let (st, headers, body) = create(&t, json!({})).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap();
    assert_eq!(headers.get(http::header::LOCATION).unwrap().to_str().unwrap(), format!("{CHATS}/{id}"));
    assert_eq!(body["model"], PREMIUM);
    assert_eq!(body["is_temporary"], false);
    assert_eq!(body["message_count"], 0);
    assert!(body.get("title").is_none(), "untitled chat must omit title: {body}");
    assert!(body.get("user_id").is_none());
    let created = time::OffsetDateTime::parse(body["created_at"].as_str().unwrap(), &time::format_description::well_known::Rfc3339);
    assert!(created.is_ok());
    assert_eq!(body["created_at"], body["updated_at"]);
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, id)| a == "create" && id.is_none()));
}

#[tokio::test]
async fn create_with_explicit_model_and_null_title() {
    let t = TestApp::new().await;
    let (st, _, body) = create(&t, json!({"model": STANDARD, "title": null})).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["model"], STANDARD);
    assert!(body.get("title").is_none());
}

#[tokio::test]
async fn create_default_falls_back_to_first_enabled_model() {
    let t = TestApp::new().await;
    t.policy.with_snapshot(|s| {
        for m in &mut s.model_catalog {
            m.preference = None;
        }
        s.model_catalog[0].enabled = false;
    });
    let (st, _, body) = create(&t, json!({})).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["model"], STANDARD);

    t.policy.with_snapshot(|s| s.model_catalog.iter_mut().for_each(|m| m.enabled = false));
    let (st, _, body) = create(&t, json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(reason(&body), "INVALID_MODEL");
}

#[tokio::test]
async fn create_rejects_disabled_and_unknown_models() {
    let t = TestApp::new().await;
    for model in ["old-model", "no-such-model"] {
        let (st, _, body) = create(&t, json!({"model": model})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["context"]["field_violations"][0]["field"], "model");
        assert_eq!(reason(&body), "INVALID_MODEL");
        assert!(body["type"].as_str().unwrap().contains("invalid_argument"));
    }
}

#[tokio::test]
async fn create_validates_title() {
    let t = TestApp::new().await;
    for bad in [json!(""), json!("   "), json!("x".repeat(256)), json!(format!("  {}  ", "y".repeat(256)))] {
        let (st, _, body) = create(&t, json!({"title": bad})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["context"]["field_violations"][0]["field"], "title");
        assert_eq!(reason(&body), "INVALID_TITLE");
        assert_eq!(body["context"]["resource_type"], CHAT_RT);
    }
    let (st, _, body) = create(&t, json!({"title": "  Hello  "})).await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(body["title"], "Hello");
    let max = "\u{e9}".repeat(255);
    let (st, _, body) = create(&t, json!({"title": format!(" {max} ")})).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["title"], max);
}

#[tokio::test]
async fn create_title_checked_before_authorization_and_model() {
    let t = TestApp::new().await;
    t.authz.deny.store(true, Ordering::SeqCst);
    let (st, _, body) = create(&t, json!({"title": " ", "model": "no-such-model"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(reason(&body), "INVALID_TITLE");
    assert!(t.authz.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_wrong_body_type_is_422() {
    let t = TestApp::new().await;
    let (st, _, body) = create(&t, json!({"title": 5})).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn authz_denied_is_403_and_pdp_outage_is_503() {
    let t = TestApp::new().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    t.authz.deny.store(true, Ordering::SeqCst);
    for (method, uri) in [
        ("POST", CHATS.to_owned()),
        ("GET", CHATS.to_owned()),
        ("GET", format!("{CHATS}/{id}")),
        ("DELETE", format!("{CHATS}/{id}")),
        ("GET", format!("{CHATS}/{id}/messages")),
    ] {
        let body = (method == "POST").then(|| json!({}));
        let (st, _, body) = t.call(&ctx_a1(), method, &uri, body).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{method} {uri}: {body}");
        assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    }
    let (st, _, body) = t.call(&ctx_a1(), "PATCH", &format!("{CHATS}/{id}"), Some(json!({"title": "x"}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{body}");

    t.authz.deny.store(false, Ordering::SeqCst);
    t.authz.unavailable.store(true, Ordering::SeqCst);
    for (method, uri) in [("POST", CHATS.to_owned()), ("GET", CHATS.to_owned()), ("GET", format!("{CHATS}/{id}"))] {
        let body = (method == "POST").then(|| json!({}));
        let (st, headers, body) = t.call(&ctx_a1(), method, &uri, body).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{body}");
        assert_eq!(headers.get(http::header::RETRY_AFTER).unwrap(), "5");
    }
}

// ───────────────────────────── get / patch ─────────────────────────────

#[tokio::test]
async fn get_reports_non_deleted_message_count() {
    let t = TestApp::new().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    insert_message(&t, id, "user", false).await;
    insert_message(&t, id, "assistant", false).await;
    insert_message(&t, id, "user", true).await;
    let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{CHATS}/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["message_count"], 2);
    assert_eq!(body["id"], id.to_string());
    assert!(body.get("messages").is_none());
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "read" && *c == Some(id)));
}

#[tokio::test]
async fn get_unknown_or_bad_id() {
    let t = TestApp::new().await;
    let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{CHATS}/{}", Uuid::new_v4()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["context"]["resource_type"], CHAT_RT);
    let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{CHATS}/not-a-uuid"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(reason(&body), "invalid_path_params");
}

#[tokio::test]
async fn patch_renames_and_ignores_model() {
    let t = TestApp::new().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    insert_message(&t, id, "user", false).await;
    let (_, _, before) = t.call(&ctx_a1(), "GET", &format!("{CHATS}/{id}"), None).await;
    let (st, _, body) =
        t.call(&ctx_a1(), "PATCH", &format!("{CHATS}/{id}"), Some(json!({"title": "  Renamed ", "model": STANDARD}))).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert_eq!(body["title"], "Renamed");
    assert_eq!(body["model"], PREMIUM);
    assert_eq!(body["message_count"], 1);
    assert_eq!(body["created_at"], before["created_at"]);
    let parse = |v: &Value| {
        time::OffsetDateTime::parse(v.as_str().unwrap(), &time::format_description::well_known::Rfc3339).unwrap()
    };
    assert!(parse(&body["updated_at"]) > parse(&before["updated_at"]));
    let (_, _, after) = t.call(&ctx_a1(), "GET", &format!("{CHATS}/{id}"), None).await;
    assert_eq!(after["title"], "Renamed");
    assert_eq!(after["updated_at"], body["updated_at"]);
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "update" && *c == Some(id)));
}

#[tokio::test]
async fn patch_title_validation() {
    let t = TestApp::new().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    let uri = format!("{CHATS}/{id}");
    for bad in [json!("   "), json!(""), json!("z".repeat(256))] {
        let (st, _, body) = t.call(&ctx_a1(), "PATCH", &uri, Some(json!({"title": bad}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(reason(&body), "INVALID_TITLE");
    }
    for bad in [json!({}), json!({"title": null}), json!({"title": 42}), json!({"model": "x"})] {
        let (st, _, body) = t.call(&ctx_a1(), "PATCH", &uri, Some(bad.clone())).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        assert_eq!(reason(&body), "invalid_json_body");
    }
    let (st, _, _) = t.call(&ctx_a1(), "PATCH", &format!("{CHATS}/{}", Uuid::new_v4()), Some(json!({"title": "ok"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

// ───────────────────────────── delete ─────────────────────────────

#[tokio::test]
async fn delete_soft_deletes_marks_attachments_and_enqueues_cleanup() {
    let mut t = TestApp::new().await;
    // Stop the outbox processor so the enqueued message stays visible in the body table.
    t.outbox.take().unwrap().stop().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    let other = t.create_chat(&ctx_a1(), None).await;
    let live = insert_attachment(&t, id, false, None).await;
    let gone = insert_attachment(&t, id, true, None).await;
    let owned = insert_attachment(&t, id, false, Some("failed")).await;
    let foreign = insert_attachment(&t, other, false, None).await;

    let (st, _, body) = t.call(&ctx_a1(), "DELETE", &format!("{CHATS}/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{body}");
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "delete" && *c == Some(id)));

    let row = attachment_row(&t, live).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.cleanup_updated_at.is_some());
    assert_eq!(attachment_row(&t, gone).await.cleanup_status, None);
    assert_eq!(attachment_row(&t, owned).await.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(attachment_row(&t, foreign).await.cleanup_status, None);

    let payloads = chat_cleanup_payloads(&t).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];
    assert_eq!(p["chat_id"], id.to_string());
    assert_eq!(p["tenant_id"], TENANT_A.to_string());
    assert_eq!(p["reason"], "chat_soft_delete");
    assert!(p["system_request_id"].as_str().unwrap().parse::<Uuid>().is_ok());
    let conn = t.app.db.conn().unwrap();
    let chat_row = chat::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(chat::Column::Id.eq(id)))
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    let deleted_at = chat_row.deleted_at.unwrap();
    assert_eq!(chat_row.updated_at, deleted_at);
    let payload_ts =
        time::OffsetDateTime::parse(p["chat_deleted_at"].as_str().unwrap(), &time::format_description::well_known::Rfc3339)
            .unwrap();
    assert_eq!(payload_ts, deleted_at);

    for (method, uri) in [("GET", format!("{CHATS}/{id}")), ("DELETE", format!("{CHATS}/{id}"))] {
        let (st, _, body) = t.call(&ctx_a1(), method, &uri, None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{method}: {body}");
        assert_eq!(body["context"]["resource_type"], CHAT_RT);
    }
    let (st, _, _) = t.call(&ctx_a1(), "PATCH", &format!("{CHATS}/{id}"), Some(json!({"title": "x"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    // The second delete enqueued nothing.
    assert_eq!(chat_cleanup_payloads(&t).await.len(), 1);
    let (_, page) = list(&t, &ctx_a1(), "").await;
    assert_eq!(ids(&page), vec![other.to_string()]);
}

// ───────────────────────────── list ─────────────────────────────

#[tokio::test]
async fn list_orders_by_activity() {
    let t = TestApp::new().await;
    let a = t.create_chat(&ctx_a1(), None).await;
    let b = t.create_chat(&ctx_a1(), None).await;
    let c = t.create_chat(&ctx_a1(), None).await;
    let (st, page) = list(&t, &ctx_a1(), "").await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(ids(&page), vec![c.to_string(), b.to_string(), a.to_string()]);
    assert_eq!(page["page_info"]["limit"], 20);
    assert!(page["page_info"]["next_cursor"].is_null());
    assert!(page["page_info"]["prev_cursor"].is_null());

    // A message activity (touch_chat in the turn transaction) moves the oldest chat first.
    t.app
        .db
        .transaction(move |tx| Box::pin(async move { touch_chat(tx, TENANT_A, a, clock::now()).await }))
        .await
        .unwrap();
    let (_, page) = list(&t, &ctx_a1(), "").await;
    assert_eq!(ids(&page), vec![a.to_string(), c.to_string(), b.to_string()]);

    // A rename bumps too.
    t.call(&ctx_a1(), "PATCH", &format!("{CHATS}/{b}"), Some(json!({"title": "b"}))).await;
    let (_, page) = list(&t, &ctx_a1(), "").await;
    assert_eq!(ids(&page), vec![b.to_string(), a.to_string(), c.to_string()]);
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, c)| a == "list" && c.is_none()));
}

#[tokio::test]
async fn list_items_carry_message_counts() {
    let t = TestApp::new().await;
    let a = t.create_chat(&ctx_a1(), None).await;
    let b = t.create_chat(&ctx_a1(), Some(STANDARD)).await;
    insert_message(&t, a, "user", false).await;
    insert_message(&t, a, "assistant", false).await;
    insert_message(&t, b, "user", true).await;
    let (_, page) = list(&t, &ctx_a1(), "").await;
    let items = page["items"].as_array().unwrap();
    let find = |id: Uuid| items.iter().find(|c| c["id"] == id.to_string()).unwrap().clone();
    assert_eq!(find(a)["message_count"], 2);
    assert_eq!(find(b)["message_count"], 0);
    assert_eq!(find(b)["model"], STANDARD);
}

async fn collect_pages(t: &TestApp, first_query: &str, extra: &str) -> Vec<Vec<String>> {
    let mut pages = Vec::new();
    let (st, mut page) = list(t, &ctx_a1(), first_query).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    loop {
        pages.push(ids(&page));
        let Some(cursor) = page["page_info"]["next_cursor"].as_str().map(str::to_owned) else { break };
        let (st, next) = list(t, &ctx_a1(), &format!("?limit=3&cursor={}{extra}", enc(&cursor))).await;
        assert_eq!(st, StatusCode::OK, "{next}");
        assert!(next["page_info"]["prev_cursor"].is_string());
        page = next;
        assert!(pages.len() < 20, "pagination does not terminate");
    }
    pages
}

#[tokio::test]
async fn list_paginates_every_chat_exactly_once() {
    let t = TestApp::new().await;
    let mut created = Vec::new();
    for _ in 0..8 {
        created.push(t.create_chat(&ctx_a1(), None).await.to_string());
    }
    let pages = collect_pages(&t, "?limit=3", "").await;
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), vec![3, 3, 2]);
    let all: Vec<String> = pages.concat();
    created.reverse();
    assert_eq!(all, created, "default order is updated_at desc across pages");

    // Ascending explicit order across pages.
    let pages = collect_pages(&t, &format!("?limit=3&$orderby={}", enc("updated_at asc")), "").await;
    created.reverse();
    assert_eq!(pages.concat(), created);

    // With a filter, the cursor carries the filter hash; the same filter must be resent.
    let since = {
        let (_, page) = list(&t, &ctx_a1(), "").await;
        page["items"][5]["updated_at"].as_str().unwrap().to_owned()
    };
    let filter = format!("&$filter={}", enc(&format!("updated_at gt {since}")));
    let pages = collect_pages(&t, &format!("?limit=3{filter}"), &filter).await;
    let all = pages.concat();
    assert_eq!(all.len(), 5, "{all:?}");
    assert_eq!(all.iter().collect::<HashSet<_>>().len(), 5);
}

#[tokio::test]
async fn list_limit_clamped_and_zero_rejected() {
    let t = TestApp::new().await;
    for _ in 0..101 {
        t.create_chat(&ctx_a1(), None).await;
    }
    let (st, page) = list(&t, &ctx_a1(), "?limit=500").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(page["items"].as_array().unwrap().len(), 100);
    assert_eq!(page["page_info"]["limit"], 100);
    assert!(page["page_info"]["next_cursor"].is_string());
    let (_, page) = list(&t, &ctx_a1(), "").await;
    assert_eq!(page["items"].as_array().unwrap().len(), 20);

    let (st, body) = list(&t, &ctx_a1(), "?limit=0").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(reason(&body), "INVALID_LIMIT");
    assert_eq!(body["context"]["resource_type"], ODATA_RT);
}

#[tokio::test]
async fn list_bad_odata_queries_are_400() {
    let t = TestApp::new().await;
    t.create_chat(&ctx_a1(), None).await;
    for (query, expected) in [
        (format!("?$filter={}", enc("foo eq 'x'")), "INVALID_FILTER"),
        (format!("?$filter={}", enc("title eq")), "INVALID_FILTER"),
        (format!("?$orderby={}", enc("model asc")), "INVALID_ORDERBY_FIELD"),
        ("?cursor=not-a-cursor".to_owned(), "INVALID_CURSOR"),
        ("?$skip=2".to_owned(), "UNSUPPORTED_QUERY_PARAM"),
    ] {
        let (st, body) = list(&t, &ctx_a1(), &query).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{query}: {body}");
        assert_eq!(body["context"]["resource_type"], ODATA_RT, "{query}: {body}");
        assert_eq!(reason(&body), expected, "{query}: {body}");
    }
    let (st, page) = list(&t, &ctx_a1(), "?$select=id").await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert!(page["items"][0]["model"].is_string(), "$select is ignored");
}

#[tokio::test]
async fn list_filters_by_title_id_and_time() {
    let t = TestApp::new().await;
    let (_, _, a) = create(&t, json!({"title": "Quarterly report"})).await;
    let (_, _, b) = create(&t, json!({"title": "Holiday plans"})).await;
    let (_, _, c) = create(&t, json!({})).await;
    let fq = |f: &str| format!("?$filter={}", enc(f));

    let (st, page) = list(&t, &ctx_a1(), &fq("contains(title, 'report')")).await;
    assert_eq!(st, StatusCode::OK, "{page}");
    assert_eq!(ids(&page), vec![a["id"].as_str().unwrap()]);
    let (_, page) = list(&t, &ctx_a1(), &fq("title eq 'Holiday plans'")).await;
    assert_eq!(ids(&page), vec![b["id"].as_str().unwrap()]);
    let (_, page) = list(&t, &ctx_a1(), &fq(&format!("id eq {}", c["id"].as_str().unwrap()))).await;
    assert_eq!(ids(&page), vec![c["id"].as_str().unwrap()]);
    let (_, page) = list(&t, &ctx_a1(), &fq(&format!("updated_at eq {}", b["updated_at"].as_str().unwrap()))).await;
    assert_eq!(ids(&page), vec![b["id"].as_str().unwrap()]);
    let (_, page) = list(&t, &ctx_a1(), &fq(&format!("updated_at ge {}", b["updated_at"].as_str().unwrap()))).await;
    assert_eq!(ids(&page), vec![c["id"].as_str().unwrap(), b["id"].as_str().unwrap()]);
    let (_, page) = list(&t, &ctx_a1(), &fq(&format!("updated_at lt {}", b["updated_at"].as_str().unwrap()))).await;
    assert_eq!(ids(&page), vec![a["id"].as_str().unwrap()]);
    let (_, page) = list(&t, &ctx_a1(), &fq("updated_at gt 2000-01-01T00:00:00Z")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 3);

    let (_, page) = list(&t, &ctx_a1(), &format!("?$orderby={}", enc("title asc"))).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 3);
}

// ───────────────────────────── isolation ─────────────────────────────

#[tokio::test]
async fn other_users_and_tenants_cannot_see_the_chat() {
    let t = TestApp::new().await;
    let id = t.create_chat(&ctx_a1(), None).await;
    let mine = t.create_chat(&ctx(USER_A2, TENANT_A), None).await;
    let theirs = t.create_chat(&ctx(USER_B1, TENANT_B), None).await;
    for other in [ctx(USER_A2, TENANT_A), ctx(USER_B1, TENANT_B)] {
        for (method, uri, body) in [
            ("GET", format!("{CHATS}/{id}"), None),
            ("PATCH", format!("{CHATS}/{id}"), Some(json!({"title": "hijack"}))),
            ("DELETE", format!("{CHATS}/{id}"), None),
            ("GET", format!("{CHATS}/{id}/messages"), None),
        ] {
            let (st, _, body) = t.call(&other, method, &uri, body).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
            assert_eq!(body["context"]["resource_type"], CHAT_RT);
        }
    }
    let (_, page) = list(&t, &ctx_a1(), "").await;
    assert_eq!(ids(&page), vec![id.to_string()]);
    let (_, page) = list(&t, &ctx(USER_A2, TENANT_A), "").await;
    assert_eq!(ids(&page), vec![mine.to_string()]);
    let (_, page) = list(&t, &ctx(USER_B1, TENANT_B), "").await;
    assert_eq!(ids(&page), vec![theirs.to_string()]);
    let (st, _, body) = t.call(&ctx_a1(), "GET", &format!("{CHATS}/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body.get("title").is_none(), "the foreign PATCH must not have renamed the chat");
}
