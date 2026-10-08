#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Chat CRUD + list (`/mini-chat/v1/chats[/{id}]`), chat delete enqueue,
//! tenant/owner isolation.

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::{
    CLOCK_START, PdpMode, TestApp, TestResponse, UserClient, attachment_row, disabled, message_row,
    premium_model, standard_model, tenant_scope, ts,
};
use mini_chat::infra::db::repos::{AttachmentRepo, ChatRepo, MessageRepo};
use serde_json::{Value, json};
use time::Duration;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const ODATA_TYPE: &str = "gts.cf.core.odata.query.v1~";
const CHAT_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn chat_path(id: &str) -> String {
    format!("{CHATS}/{id}")
}

fn rfc3339(unix: i64) -> String {
    ts(unix).format(&Rfc3339).unwrap()
}

fn rfc3339_millis(unix: i64, millis: i64) -> String {
    (ts(unix) + Duration::milliseconds(millis))
        .format(&Rfc3339)
        .unwrap()
}

fn rfc3339_micros(unix: i64, micros: i64) -> String {
    (ts(unix) + Duration::microseconds(micros))
        .format(&Rfc3339)
        .unwrap()
}

/// First `field_violations[]` entry of a Problem body: `(field, reason)`.
fn violation(body: &Value) -> (String, String) {
    let v = &body["context"]["field_violations"][0];
    (
        v["field"].as_str().unwrap_or_default().to_owned(),
        v["reason"].as_str().unwrap_or_default().to_owned(),
    )
}

fn assert_invalid(resp: &TestResponse, field: &str, reason: &str) {
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    let body = resp.json();
    assert_eq!(
        violation(&body),
        (field.to_owned(), reason.to_owned()),
        "{body}"
    );
}

fn assert_chat_404(resp: &TestResponse) {
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
    assert_eq!(resp.json()["context"]["resource_type"], CHAT_TYPE);
}

async fn create(client: &UserClient<'_>, body: &Value) -> Value {
    let resp = client.post_json(CHATS, body).await;
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    resp.json()
}

fn item_ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn raw_request(
    client: &UserClient<'_>,
    method: Method,
    path: &str,
    content_type: Option<&str>,
    body: &str,
) -> TestResponse {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(ct) = content_type {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    client
        .send(req.body(Body::from(body.to_owned())).unwrap())
        .await
}

// ---------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_returns_201_with_location_and_default_model() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();

    let resp = app.as_user(user, tenant).post_json(CHATS, &json!({})).await;

    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    let body = resp.json();
    let id = body["id"].as_str().expect("id");
    Uuid::parse_str(id).expect("uuid id");
    assert_eq!(
        resp.header("location").as_deref(),
        Some(format!("/mini-chat/v1/chats/{id}").as_str())
    );
    // Default catalog: p1 (premium, no preference), s1 (is_default), s-novision.
    assert_eq!(body["model"], "s1");
    let obj = body.as_object().unwrap();
    assert!(
        !obj.contains_key("title"),
        "untitled chat has no title key: {body}"
    );
    assert!(!obj.contains_key("user_id"), "{body}");
    assert!(!obj.contains_key("tenant_id"), "{body}");
    assert_eq!(body["message_count"], 0);
    assert_eq!(body["is_temporary"], false);
    assert_eq!(body["created_at"], rfc3339(CLOCK_START));
    assert_eq!(body["updated_at"], rfc3339(CLOCK_START));

    let stored = ChatRepo
        .find_by_id(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            Uuid::parse_str(id).unwrap(),
        )
        .await
        .unwrap()
        .expect("row stored for the caller's tenant + owner");
    assert_eq!(stored.model, "s1");
}

#[tokio::test]
async fn create_with_explicit_model_and_title() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();

    let body = create(
        &app.as_user(user, tenant),
        &json!({"title": "Q3", "model": "p1"}),
    )
    .await;

    assert_eq!(body["model"], "p1");
    assert_eq!(body["title"], "Q3");
}

#[tokio::test]
async fn create_with_unknown_or_disabled_model_is_400_invalid_model() {
    let app = TestApp::builder()
        .catalog(vec![standard_model("s1"), disabled(premium_model("p-off"))])
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    for model in ["unknown", "p-off"] {
        let resp = client.post_json(CHATS, &json!({"model": model})).await;
        assert_invalid(&resp, "model", "INVALID_MODEL");
        assert_eq!(resp.json()["context"]["resource_type"], CHAT_TYPE);
    }
}

#[tokio::test]
async fn create_without_enabled_models_is_400_invalid_model() {
    let app = TestApp::builder()
        .catalog(vec![disabled(standard_model("s1"))])
        .build()
        .await;
    let (user, tenant) = ids();

    let resp = app.as_user(user, tenant).post_json(CHATS, &json!({})).await;

    assert_invalid(&resp, "model", "INVALID_MODEL");
}

#[tokio::test]
async fn title_validation() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    let resp = client.post_json(CHATS, &json!({"title": "   "})).await;
    assert_invalid(&resp, "title", "INVALID_TITLE");

    let resp = client
        .post_json(CHATS, &json!({"title": "a".repeat(256)}))
        .await;
    assert_invalid(&resp, "title", "INVALID_TITLE");

    let body = create(&client, &json!({"title": "  hi  "})).await;
    assert_eq!(body["title"], "hi");

    // Length is counted in characters, not bytes (255 × the 2-byte "é").
    let accented = "\u{e9}".repeat(255);
    let body = create(&client, &json!({"title": accented})).await;
    assert_eq!(body["title"], accented);
    let resp = client
        .post_json(CHATS, &json!({"title": "\u{e9}".repeat(256)}))
        .await;
    assert_invalid(&resp, "title", "INVALID_TITLE");

    // Title is validated before the model lookup...
    let resp = client
        .post_json(CHATS, &json!({"title": "", "model": "unknown"}))
        .await;
    assert_invalid(&resp, "title", "INVALID_TITLE");

    // ...and before the authorization check.
    app.pdp.set_mode(PdpMode::Deny);
    let resp = client.post_json(CHATS, &json!({"title": " "})).await;
    assert_invalid(&resp, "title", "INVALID_TITLE");
}

#[tokio::test]
async fn create_pdp_deny_is_403() {
    let app = TestApp::builder().build().await;
    app.pdp.set_mode(PdpMode::Deny);
    let (user, tenant) = ids();

    let resp = app.as_user(user, tenant).post_json(CHATS, &json!({})).await;

    assert_eq!(resp.status, StatusCode::FORBIDDEN, "{}", resp.text());
    let req = app.pdp.last_request();
    assert_eq!(req.action.name, "create");
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patch_renames_and_ignores_model() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({"title": "Original"})).await;
    let path = chat_path(created["id"].as_str().unwrap());
    app.clock.advance(Duration::seconds(60));

    let resp = client
        .patch_json(&path, &json!({"title": "  Renamed ", "model": "p1"}))
        .await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["title"], "Renamed");
    assert_eq!(body["model"], "s1");
    assert_eq!(body["id"], created["id"]);
    assert_eq!(body["created_at"], rfc3339(CLOCK_START));
    assert_eq!(body["updated_at"], rfc3339(CLOCK_START + 60));

    let fetched = client.get(&path).await.json();
    assert_eq!(fetched["title"], "Renamed");
    assert_eq!(fetched["model"], "s1");
    assert_eq!(fetched["updated_at"], rfc3339(CLOCK_START + 60));
}

#[tokio::test]
async fn patch_invalid_title_is_400() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({"title": "Original"})).await;
    let path = chat_path(created["id"].as_str().unwrap());

    for title in [" \t ".to_owned(), "x".repeat(256)] {
        let resp = client.patch_json(&path, &json!({ "title": title })).await;
        assert_invalid(&resp, "title", "INVALID_TITLE");
    }
    assert_eq!(client.get(&path).await.json()["title"], "Original");
}

#[tokio::test]
async fn patch_missing_title_is_422() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({})).await;
    let path = chat_path(created["id"].as_str().unwrap());

    for body in [json!({}), json!({"title": null}), json!({"title": 7})] {
        let resp = client.patch_json(&path, &body).await;
        assert_eq!(
            resp.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            resp.text()
        );
        assert_eq!(
            violation(&resp.json()),
            ("body".to_owned(), "invalid_json_body".to_owned())
        );
    }
}

#[tokio::test]
async fn patch_without_json_content_type_is_415() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({})).await;
    let path = chat_path(created["id"].as_str().unwrap());

    let resp = raw_request(&client, Method::PATCH, &path, None, r#"{"title":"x"}"#).await;
    assert_eq!(
        resp.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        resp.text()
    );
    assert_eq!(
        violation(&resp.json()),
        ("body".to_owned(), "missing_json_content_type".to_owned())
    );

    let resp = raw_request(
        &client,
        Method::POST,
        CHATS,
        Some("text/plain"),
        r#"{"title":"x"}"#,
    )
    .await;
    assert_eq!(resp.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn malformed_json_is_400() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({})).await;
    let path = chat_path(created["id"].as_str().unwrap());

    for (method, p) in [(Method::POST, CHATS.to_owned()), (Method::PATCH, path)] {
        let resp = raw_request(&client, method, &p, Some("application/json"), "{\"title\":").await;
        assert_invalid(&resp, "body", "json_syntax_error");
    }
}

// ---------------------------------------------------------------------------
// Get / delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_returns_message_count_of_non_deleted_messages() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({"title": "T"})).await;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let conn = app.db.conn().unwrap();
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .find_by_id(&conn, &scope, id)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..2 {
        MessageRepo
            .insert(
                &conn,
                &scope,
                message_row(&chat, Some(Uuid::new_v4()), "user"),
            )
            .await
            .unwrap();
    }
    let mut gone = message_row(&chat, Some(Uuid::new_v4()), "assistant");
    gone.deleted_at = Some(ts(CLOCK_START));
    MessageRepo.insert(&conn, &scope, gone).await.unwrap();
    // A message of another chat of the same user is not counted.
    let other = create(&client, &json!({})).await;
    let other_chat = ChatRepo
        .find_by_id(
            &conn,
            &scope,
            Uuid::parse_str(other["id"].as_str().unwrap()).unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    MessageRepo
        .insert(&conn, &scope, message_row(&other_chat, None, "user"))
        .await
        .unwrap();

    let resp = client.get(&chat_path(&id.to_string())).await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["message_count"], 2);
    assert_eq!(body["title"], "T");
    assert!(body.get("messages").is_none());

    let page = client.get(CHATS).await.json();
    let listed = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == created["id"])
        .unwrap()
        .clone();
    assert_eq!(listed["message_count"], 2);
}

#[tokio::test]
async fn get_and_delete() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({"title": "Doomed"})).await;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let path = chat_path(&id.to_string());
    let conn = app.db.conn().unwrap();
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .find_by_id(&conn, &scope, id)
        .await
        .unwrap()
        .unwrap();
    let live = AttachmentRepo
        .insert(&conn, &scope, attachment_row(&chat))
        .await
        .unwrap();
    let mut already_deleted = attachment_row(&chat);
    already_deleted.deleted_at = Some(ts(CLOCK_START));
    let already_deleted = AttachmentRepo
        .insert(&conn, &scope, already_deleted)
        .await
        .unwrap();
    let mut already_failed = attachment_row(&chat);
    already_failed.cleanup_status = Some("failed".to_owned());
    let already_failed = AttachmentRepo
        .insert(&conn, &scope, already_failed)
        .await
        .unwrap();

    assert_eq!(client.get(&path).await.status, StatusCode::OK);
    app.clock.advance(Duration::seconds(30));
    let now = ts(CLOCK_START + 30);

    let resp = client.delete(&path).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    assert!(resp.body.is_empty());

    assert_chat_404(&client.get(&path).await);
    assert_chat_404(&client.delete(&path).await);
    assert_chat_404(&client.patch_json(&path, &json!({"title": "x"})).await);
    assert!(item_ids(&client.get(CHATS).await.json()).is_empty());

    // Soft delete: the row stays with deleted_at = updated_at = now.
    let row = ChatRepo
        .find_by_id(&conn, &scope, id)
        .await
        .unwrap()
        .expect("soft-deleted row kept");
    assert_eq!(row.deleted_at, Some(now));
    assert_eq!(row.updated_at, now);

    // Attachments: only non-deleted ones without a cleanup status are marked.
    let live = AttachmentRepo
        .find_by_id(&conn, &scope, live.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(live.cleanup_updated_at, Some(now));
    assert_eq!(
        live.deleted_at, None,
        "attachment rows are not soft-deleted"
    );
    let already_deleted = AttachmentRepo
        .find_by_id(&conn, &scope, already_deleted.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(already_deleted.cleanup_status, None);
    assert_eq!(already_deleted.cleanup_updated_at, None);
    let already_failed = AttachmentRepo
        .find_by_id(&conn, &scope, already_failed.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(already_failed.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(already_failed.cleanup_updated_at, None);

    // Exactly one chat-cleanup message (the failed second delete enqueued nothing).
    let payloads = app
        .outbox_payloads(&app.config.outbox.chat_cleanup_queue_name)
        .await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];
    assert_eq!(p["reason"], "chat_soft_delete");
    assert_eq!(p["chat_id"], json!(id));
    assert_eq!(p["tenant_id"], json!(tenant));
    assert_eq!(p["chat_deleted_at"], rfc3339(CLOCK_START + 30));
    Uuid::parse_str(p["system_request_id"].as_str().unwrap()).expect("system_request_id");
}

#[tokio::test]
async fn delete_pdp_failure_is_503_and_keeps_chat() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let created = create(&client, &json!({})).await;
    let path = chat_path(created["id"].as_str().unwrap());

    app.pdp.set_mode(PdpMode::Fail);
    let resp = client.delete(&path).await;
    assert_eq!(
        resp.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        resp.text()
    );
    let req = app.pdp.last_request();
    assert_eq!(req.action.name, "delete");
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    assert_eq!(req.resource.id, Some(id));

    app.pdp.set_mode(PdpMode::Allow);
    assert_eq!(client.get(&path).await.status, StatusCode::OK);
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_orders_by_updated_at_desc_and_paginates() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let mut created = Vec::new();
    for title in ["one", "two", "three"] {
        created.push(create(&client, &json!({ "title": title })).await["id"].clone());
        app.clock.advance(Duration::seconds(10));
    }
    // Renaming "one" makes it the most recently active chat.
    let resp = client
        .patch_json(
            &chat_path(created[0].as_str().unwrap()),
            &json!({"title": "one!"}),
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK);
    let expected: Vec<String> = [&created[0], &created[2], &created[1]]
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();

    let full = client.get(CHATS).await;
    assert_eq!(full.status, StatusCode::OK, "{}", full.text());
    let full = full.json();
    assert_eq!(item_ids(&full), expected);
    assert_eq!(full["page_info"]["limit"], 20);
    assert!(full["page_info"]["next_cursor"].is_null());

    let first = client.get(&format!("{CHATS}?limit=2")).await.json();
    assert_eq!(item_ids(&first), expected[..2]);
    assert_eq!(first["page_info"]["limit"], 2);
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .expect("next_cursor on a full page");

    let second = client
        .get(&format!("{CHATS}?limit=2&cursor={cursor}"))
        .await;
    assert_eq!(second.status, StatusCode::OK, "{}", second.text());
    let second = second.json();
    assert_eq!(item_ids(&second), expected[2..]);
    assert!(second["page_info"]["next_cursor"].is_null());
}

#[tokio::test]
async fn list_orderby_title() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let b = create(&client, &json!({"title": "b"})).await;
    let a = create(&client, &json!({"title": "a"})).await;
    let c = create(&client, &json!({"title": "c"})).await;

    let resp = client.get(&format!("{CHATS}?$orderby=title%20asc")).await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let got = item_ids(&resp.json());
    let want: Vec<String> = [a, b, c]
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn list_limit_over_100_clamped() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    create(&client, &json!({})).await;

    let resp = client.get(&format!("{CHATS}?limit=500")).await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let body = resp.json();
    assert_eq!(body["page_info"]["limit"], 100);
    assert_eq!(item_ids(&body).len(), 1);
}

#[tokio::test]
async fn list_limit_zero_is_400() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();

    let resp = app
        .as_user(user, tenant)
        .get(&format!("{CHATS}?limit=0"))
        .await;

    assert_invalid(&resp, "$top", "INVALID_LIMIT");
    assert_eq!(resp.json()["context"]["resource_type"], ODATA_TYPE);
}

/// Four chats updated at `CLOCK_START` + 0 s, 0.25 s, 0.5 s and 1 s (stored
/// timestamps with and without fractional seconds). Returns their ids and
/// `updated_at` values as returned by the API, oldest first.
async fn chats_at_fractional_instants(
    app: &TestApp,
    client: &UserClient<'_>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for millis in [0_i64, 250, 500, 1000] {
        app.clock
            .set(ts(CLOCK_START) + Duration::milliseconds(millis));
        let body = create(client, &json!({})).await;
        out.push((
            body["id"].as_str().unwrap().to_owned(),
            body["updated_at"].as_str().unwrap().to_owned(),
        ));
    }
    out
}

async fn filtered_ids(client: &UserClient<'_>, filter: &str) -> Vec<String> {
    let encoded = filter.replace(' ', "%20");
    let resp = client.get(&format!("{CHATS}?$filter={encoded}")).await;
    assert_eq!(resp.status, StatusCode::OK, "{filter}: {}", resp.text());
    item_ids(&resp.json())
}

#[tokio::test]
async fn list_orders_and_pages_fractional_timestamps() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chats = chats_at_fractional_instants(&app, &client).await;
    let newest_first: Vec<String> = chats.iter().rev().map(|(id, _)| id.clone()).collect();

    assert_eq!(item_ids(&client.get(CHATS).await.json()), newest_first);

    let mut paged = Vec::new();
    let mut url = format!("{CHATS}?limit=1");
    for _ in 0..=newest_first.len() {
        let page = client.get(&url).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        let page = page.json();
        paged.extend(item_ids(&page));
        match page["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{CHATS}?limit=1&cursor={c}"),
            None => break,
        }
    }
    // A broken cursor that never ends shows up as extra pages, not a hang.
    assert_eq!(paged, newest_first);
}

#[tokio::test]
async fn list_filter_updated_at_exact_and_ranges() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chats = chats_at_fractional_instants(&app, &client).await;
    let id = |i: usize| chats[i].0.clone();
    let at = |i: usize| chats[i].1.clone();
    assert_eq!(at(1), rfc3339_millis(CLOCK_START, 250));

    // Exact stored instants (whole and fractional seconds).
    for i in 0..4 {
        assert_eq!(
            filtered_ids(&client, &format!("updated_at eq {}", at(i))).await,
            [id(i)],
            "eq {}",
            at(i)
        );
    }
    assert_eq!(
        filtered_ids(&client, &format!("updated_at ne {}", at(1))).await,
        [id(3), id(2), id(0)]
    );
    // Open range excludes both bounds.
    assert_eq!(
        filtered_ids(
            &client,
            &format!("updated_at gt {} and updated_at lt {}", at(0), at(3))
        )
        .await,
        [id(2), id(1)]
    );
    // Closed bounds include the row at the exact instant.
    assert_eq!(
        filtered_ids(&client, &format!("updated_at le {}", at(1))).await,
        [id(1), id(0)]
    );
    assert_eq!(
        filtered_ids(&client, &format!("updated_at ge {}", at(2))).await,
        [id(3), id(2)]
    );
    // A fractional value between stored instants.
    let between = rfc3339_millis(CLOCK_START, 300);
    assert_eq!(
        filtered_ids(&client, &format!("updated_at gt {between}")).await,
        [id(3), id(2)]
    );
    assert_eq!(
        filtered_ids(&client, &format!("updated_at lt {between}")).await,
        [id(1), id(0)]
    );
    // Sub-millisecond precision: 0.250001 s is after the 0.25 s row.
    let just_after = rfc3339_micros(CLOCK_START, 250_001);
    assert_eq!(
        filtered_ids(&client, &format!("updated_at lt {just_after}")).await,
        [id(1), id(0)]
    );
    assert!(
        filtered_ids(&client, &format!("updated_at eq {just_after}"))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn list_filter_contains_title() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let weekly = create(&client, &json!({"title": "Weekly report"})).await;
    create(&client, &json!({"title": "Notes"})).await;
    create(&client, &json!({})).await;
    app.clock.advance(Duration::seconds(5));
    let quarterly = create(&client, &json!({"title": "Quarterly report"})).await;

    let resp = client
        .get(&format!("{CHATS}?$filter=contains(title,%27report%27)"))
        .await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(
        item_ids(&resp.json()),
        [
            quarterly["id"].as_str().unwrap(),
            weekly["id"].as_str().unwrap()
        ]
    );
}

#[tokio::test]
async fn list_unknown_field_is_400_odata_resource_type() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    create(&client, &json!({})).await;

    let resp = client
        .get(&format!("{CHATS}?$filter=model%20eq%20%27s1%27"))
        .await;
    assert_invalid(&resp, "$filter", "INVALID_FILTER");
    assert_eq!(resp.json()["context"]["resource_type"], ODATA_TYPE);

    let resp = client
        .get(&format!("{CHATS}?$orderby=message_count%20desc"))
        .await;
    assert_invalid(&resp, "$orderby", "INVALID_ORDERBY_FIELD");
    assert_eq!(resp.json()["context"]["resource_type"], ODATA_TYPE);

    let resp = client.get(&format!("{CHATS}?$filter=title%20eq")).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    assert_eq!(resp.json()["context"]["resource_type"], ODATA_TYPE);
}

#[tokio::test]
async fn list_bad_cursor_is_400() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();

    let resp = app
        .as_user(user, tenant)
        .get(&format!("{CHATS}?cursor=not-a-cursor"))
        .await;

    assert_invalid(&resp, "cursor", "INVALID_CURSOR");
    assert_eq!(resp.json()["context"]["resource_type"], ODATA_TYPE);
}

#[tokio::test]
async fn list_select_accepted_and_ignored() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    create(&client, &json!({"title": "T"})).await;

    let resp = client.get(&format!("{CHATS}?$select=id")).await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let item = resp.json()["items"][0].clone();
    for key in [
        "id",
        "model",
        "title",
        "is_temporary",
        "message_count",
        "created_at",
        "updated_at",
    ] {
        assert!(item.get(key).is_some(), "{key} missing from {item}");
    }
}

// ---------------------------------------------------------------------------
// Isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn isolation() {
    let app = TestApp::builder().build().await;
    let tenant = Uuid::new_v4();
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    let (carol, other_tenant) = ids();
    let a = app.as_user(alice, tenant);
    let created = create(&a, &json!({"title": "Alice's"})).await;
    let path = chat_path(created["id"].as_str().unwrap());

    for intruder in [app.as_user(bob, tenant), app.as_user(carol, other_tenant)] {
        assert_chat_404(&intruder.get(&path).await);
        assert_chat_404(&intruder.patch_json(&path, &json!({"title": "mine"})).await);
        assert_chat_404(&intruder.delete(&path).await);
        let page = intruder.get(CHATS).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text());
        assert!(item_ids(&page.json()).is_empty());
    }

    let mine = a.get(&path).await;
    assert_eq!(mine.status, StatusCode::OK);
    assert_eq!(mine.json()["title"], "Alice's");
    assert_eq!(item_ids(&a.get(CHATS).await.json()).len(), 1);
}

#[tokio::test]
async fn non_uuid_path_is_400_invalid_path_params() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let path = chat_path("not-a-uuid");

    for resp in [
        client.get(&path).await,
        client.patch_json(&path, &json!({"title": "x"})).await,
        client.delete(&path).await,
    ] {
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
        assert_eq!(violation(&resp.json()).1, "invalid_path_params");
    }
}

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_routes_registered_with_operation_ids() {
    let app = TestApp::builder().build().await;
    let ops = app.operations();
    let find = |method: &str, path: &str| {
        ops.iter()
            .find(|o| o.method == method && o.path == path)
            .unwrap_or_else(|| panic!("{method} {path} not registered"))
    };
    for (method, path, op_id) in [
        ("POST", CHATS, "mini_chat.create_chat"),
        ("GET", CHATS, "mini_chat.list_chats"),
        ("GET", "/mini-chat/v1/chats/{id}", "mini_chat.get_chat"),
        ("PATCH", "/mini-chat/v1/chats/{id}", "mini_chat.update_chat"),
        (
            "DELETE",
            "/mini-chat/v1/chats/{id}",
            "mini_chat.delete_chat",
        ),
    ] {
        let op = find(method, path);
        assert_eq!(op.operation_id.as_deref(), Some(op_id));
        assert!(op.authenticated);
        assert!(op.license_requirement.is_some());
    }
    let create_op = find("POST", CHATS);
    let created = create_op
        .responses
        .iter()
        .find(|r| r.status == 201)
        .expect("201 response");
    assert!(
        created
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("location")),
        "Location header declared on 201"
    );
    let list_params: Vec<&str> = find("GET", CHATS)
        .params
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    for p in ["limit", "cursor", "$filter", "$orderby"] {
        assert!(
            list_params.contains(&p),
            "{p} not declared: {list_params:?}"
        );
    }
    assert!(!list_params.contains(&"$select"), "$select is not declared");
}
