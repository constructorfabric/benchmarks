//! Chats CRUD over REST (DESIGN §3.3 Create/List/Get/Update/Delete Chat, §3.8).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use mini_chat::infra::db::entities::{attachment, chat};
use mini_chat::infra::outbox::QueueKind;
use mini_chat::testing::{PdpMode, TestApp, TestResponse, TestUser, catalog};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter};
use serde_json::{Value, json};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
/// Two UTF-8 bytes each.
const E_ACUTE: &str = "\u{e9}";
const U_UMLAUT: &str = "\u{fc}";

async fn app() -> TestApp {
    TestApp::builder().build().await
}

async fn create(app: &TestApp, user: TestUser, body: Value) -> TestResponse {
    app.call(user, Method::POST, CHATS, Some(body)).await
}

async fn create_ok(app: &TestApp, user: TestUser, body: Value) -> Value {
    let r = create(app, user, body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    r.json
}

fn chat_path(id: &str) -> String {
    format!("{CHATS}/{id}")
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().expect("id").to_owned()
}

/// Asserts a 4xx problem with exactly one `field_violations` entry.
fn assert_field_violation(r: &TestResponse, status: StatusCode, field: &str, reason: &str) {
    assert_eq!(r.status, status, "{}", r.json);
    let v = &r.json["context"]["field_violations"];
    assert_eq!(v.as_array().map(Vec::len), Some(1), "{}", r.json);
    assert_eq!(v[0]["field"], field, "{}", r.json);
    assert_eq!(v[0]["reason"], reason, "{}", r.json);
}

async fn chat_row(app: &TestApp, id: &str) -> chat::Model {
    let id = Uuid::parse_str(id).unwrap();
    let conn = app.db.conn().unwrap();
    chat::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("chat row")
}

async fn send(app: &TestApp, req: Request<Body>) -> TestResponse {
    TestResponse::read(app.raw(req).await).await
}

fn json_request(
    method: Method,
    uri: &str,
    content_type: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    let mut req = b.body(Body::from(body.to_owned())).unwrap();
    req.extensions_mut().insert(TestUser::A1);
    req
}

// ---------------------------------------------------------------------------------------------
// Create
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_chat_defaults_model_and_returns_201_location() {
    let app = app().await;
    let r = create(&app, TestUser::A1, json!({})).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = id_of(&r.json);
    Uuid::parse_str(&id).unwrap();
    assert_eq!(r.json["model"], "gpt-premium");
    assert_eq!(
        r.headers.get("location").unwrap().to_str().unwrap(),
        format!("/mini-chat/v1/chats/{id}")
    );
    assert_eq!(r.json["message_count"], 0);
    assert_eq!(r.json["is_temporary"], false);
    assert!(r.json.get("title").is_none(), "{}", r.json);
    assert!(r.json.get("user_id").is_none(), "{}", r.json);
    assert_eq!(r.json["created_at"], r.json["updated_at"]);

    let row = chat_row(&app, &id).await;
    assert_eq!(row.tenant_id, TestUser::A1.tenant_id);
    assert_eq!(row.user_id, TestUser::A1.user_id);
    assert_eq!(row.model.as_deref(), Some("gpt-premium"));
    assert!(row.title.is_none());
    assert!(row.deleted_at.is_none());
}

#[tokio::test]
async fn create_chat_with_title_trims() {
    let app = app().await;
    let v = create_ok(
        &app,
        TestUser::A1,
        json!({"title": "  Q3 report \n", "model": "gpt-standard"}),
    )
    .await;
    assert_eq!(v["title"], "Q3 report");
    assert_eq!(v["model"], "gpt-standard");

    let null_title = create_ok(&app, TestUser::A1, json!({"title": null, "model": null})).await;
    assert!(null_title.get("title").is_none());
    assert_eq!(null_title["model"], "gpt-premium");
}

#[tokio::test]
async fn title_255_multibyte_chars_accepted_256_rejected() {
    let app = app().await;
    let ok = E_ACUTE.repeat(255);
    let v = create_ok(&app, TestUser::A1, json!({ "title": ok })).await;
    assert_eq!(v["title"], ok);

    let too_long = E_ACUTE.repeat(256);
    let r = create(&app, TestUser::A1, json!({ "title": too_long })).await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "title", "INVALID_TITLE");

    // Same limits on rename.
    let id = id_of(&v);
    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&id),
            Some(json!({ "title": E_ACUTE.repeat(256) })),
        )
        .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "title", "INVALID_TITLE");
    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&id),
            Some(json!({ "title": U_UMLAUT.repeat(255) })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
}

#[tokio::test]
async fn whitespace_title_rejected() {
    let app = app().await;
    for title in ["", "   ", "\t\n "] {
        let r = create(&app, TestUser::A1, json!({ "title": title })).await;
        assert_field_violation(&r, StatusCode::BAD_REQUEST, "title", "INVALID_TITLE");
    }
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&id),
            Some(json!({ "title": "  " })),
        )
        .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "title", "INVALID_TITLE");
}

#[tokio::test]
async fn invalid_title_checked_before_model() {
    let app = app().await;
    let r = create(
        &app,
        TestUser::A1,
        json!({ "title": " ", "model": "no-such-model" }),
    )
    .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "title", "INVALID_TITLE");

    // ... and before the authorization check.
    let denied = TestApp::builder().pdp(PdpMode::Deny).build().await;
    let r = create(&denied, TestUser::A1, json!({ "title": "" })).await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "title", "INVALID_TITLE");
}

#[tokio::test]
async fn create_chat_unknown_or_disabled_model_is_invalid_model() {
    let mut disabled = catalog::standard_model("gpt-old");
    disabled.enabled = false;
    let mut models = catalog::default_catalog();
    models.push(disabled);
    let app = TestApp::builder().catalog(models).build().await;

    for model in ["no-such-model", "gpt-old"] {
        let r = create(&app, TestUser::A1, json!({ "model": model })).await;
        assert_field_violation(&r, StatusCode::BAD_REQUEST, "model", "INVALID_MODEL");
    }
}

#[tokio::test]
async fn default_falls_back_to_first_enabled_when_no_is_default() {
    let mut off = catalog::premium_model("gpt-off");
    off.enabled = false;
    let a = catalog::standard_model("gpt-a");
    let b = catalog::premium_model("gpt-b");
    let app = TestApp::builder().catalog(vec![off, a, b]).build().await;
    let v = create_ok(&app, TestUser::A1, json!({})).await;
    assert_eq!(v["model"], "gpt-a");

    // An enabled `is_default` entry wins over catalog order; a disabled one is skipped.
    let mut off_default = catalog::premium_model("gpt-off-default");
    off_default.enabled = false;
    off_default.preference = Some(mini_chat_sdk::ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let mut b_default = catalog::premium_model("gpt-b");
    b_default.preference = Some(mini_chat_sdk::ModelPreference {
        is_default: true,
        sort_order: 1,
    });
    let app = TestApp::builder()
        .catalog(vec![
            off_default,
            catalog::standard_model("gpt-a"),
            b_default,
        ])
        .build()
        .await;
    let v = create_ok(&app, TestUser::A1, json!({})).await;
    assert_eq!(v["model"], "gpt-b");
}

// ---------------------------------------------------------------------------------------------
// Get
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_chat_foreign_user_and_tenant_are_404() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({"title": "mine"})).await);

    let r = app
        .call(TestUser::A1, Method::GET, &chat_path(&id), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["id"], id);
    assert_eq!(r.json["title"], "mine");
    assert_eq!(r.json["message_count"], 0);

    for user in [TestUser::A2, TestUser::B1] {
        let r = app.call(user, Method::GET, &chat_path(&id), None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
        assert_eq!(
            r.json["context"]["resource_type"],
            "gts.cf.core.mini_chat.chat.v1~"
        );
        let r = app.call(user, Method::DELETE, &chat_path(&id), None).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
        let r = app
            .call(
                user,
                Method::PATCH,
                &chat_path(&id),
                Some(json!({"title": "stolen"})),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    }
    let row = chat_row(&app, &id).await;
    assert_eq!(row.title.as_deref(), Some("mine"));
    assert!(
        row.deleted_at.is_none(),
        "foreign DELETE must not soft-delete"
    );
    app.assert_no_outbox(QueueKind::ChatCleanup, Duration::from_millis(300))
        .await;

    let r = app
        .call(
            TestUser::A1,
            Method::GET,
            &chat_path(&Uuid::new_v4().to_string()),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
}

#[tokio::test]
async fn get_deleted_chat_404() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let r = app
        .call(TestUser::A1, Method::DELETE, &chat_path(&id), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    let r = app
        .call(TestUser::A1, Method::GET, &chat_path(&id), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&id),
            Some(json!({"title": "x"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
}

// ---------------------------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn patch_title_updates_updated_at_and_ignores_model() {
    let app = app().await;
    let created = create_ok(&app, TestUser::A1, json!({"title": "old"})).await;
    let id = id_of(&created);
    let before = chat_row(&app, &id).await;

    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&id),
            Some(json!({"title": "  X  ", "model": "gpt-standard", "is_temporary": true})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["title"], "X");
    assert_eq!(r.json["model"], "gpt-premium");
    assert_eq!(r.json["is_temporary"], false);
    assert_eq!(r.json["created_at"], created["created_at"]);
    assert_eq!(r.json["message_count"], 0);

    let after = chat_row(&app, &id).await;
    assert!(after.updated_at > before.updated_at);
    assert_eq!(after.created_at, before.created_at);
    assert_eq!(after.model.as_deref(), Some("gpt-premium"));
    assert_eq!(after.title.as_deref(), Some("X"));
}

#[tokio::test]
async fn patch_missing_title_is_422() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&id),
            Some(json!({"model": "x"})),
        )
        .await;
    assert_field_violation(
        &r,
        StatusCode::UNPROCESSABLE_ENTITY,
        "body",
        "invalid_json_body",
    );
}

#[tokio::test]
async fn patch_null_title_is_422() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    for body in [json!({"title": null}), json!({"title": 5})] {
        let r = app
            .call(TestUser::A1, Method::PATCH, &chat_path(&id), Some(body))
            .await;
        assert_field_violation(
            &r,
            StatusCode::UNPROCESSABLE_ENTITY,
            "body",
            "invalid_json_body",
        );
    }
}

#[tokio::test]
async fn malformed_json_is_400_json_syntax_error() {
    let app = app().await;
    let r = send(
        &app,
        json_request(
            Method::POST,
            CHATS,
            Some("application/json"),
            "{\"title\": ",
        ),
    )
    .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "body", "json_syntax_error");
}

#[tokio::test]
async fn missing_content_type_is_415() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let r = send(
        &app,
        json_request(Method::PATCH, &chat_path(&id), None, "{\"title\":\"x\"}"),
    )
    .await;
    assert_field_violation(
        &r,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "body",
        "missing_json_content_type",
    );
    let r = send(&app, json_request(Method::POST, CHATS, None, "{}")).await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{}", r.json);
}

#[tokio::test]
async fn non_uuid_path_is_400_invalid_path_params() {
    let app = app().await;
    for method in [Method::GET, Method::DELETE] {
        let r = app
            .call(TestUser::A1, method, &chat_path("not-a-uuid"), None)
            .await;
        assert_field_violation(&r, StatusCode::BAD_REQUEST, "path", "invalid_path_params");
    }
    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path("not-a-uuid"),
            Some(json!({"title": "x"})),
        )
        .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "path", "invalid_path_params");
}

// ---------------------------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn delete_chat_204_then_404_and_enqueues_chat_cleanup() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let before = chat_row(&app, &id).await;

    let r = app
        .call(TestUser::A1, Method::DELETE, &chat_path(&id), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    assert_eq!(r.json, Value::Null);

    let row = chat_row(&app, &id).await;
    let deleted_at = row.deleted_at.expect("deleted_at set");
    assert_eq!(row.updated_at, deleted_at);
    assert!(row.updated_at > before.updated_at);

    let r = app
        .call(TestUser::A1, Method::DELETE, &chat_path(&id), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);

    let msgs = app.outbox_messages_n(QueueKind::ChatCleanup, 1).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    // No late duplicate (the second, 404 DELETE enqueues nothing either).
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(app.outbox_messages(QueueKind::ChatCleanup).await, msgs);
    let m = &msgs[0];
    assert_eq!(m["reason"], "chat_soft_delete");
    assert_eq!(m["chat_id"], id);
    assert_eq!(m["tenant_id"], TestUser::A1.tenant_id.to_string());
    Uuid::parse_str(m["system_request_id"].as_str().unwrap()).unwrap();
    assert!(m["chat_deleted_at"].is_string(), "{m}");
}

/// Inserts a ready document attachment of `chat_id` (A1's tenant) and returns its id.
async fn seed_attachment(
    app: &TestApp,
    chat_id: &str,
    deleted: bool,
    cleanup_status: Option<&str>,
) -> Uuid {
    let now = chrono::Utc::now();
    let id = Uuid::new_v4();
    let row = attachment::Model {
        id,
        tenant_id: TestUser::A1.tenant_id,
        chat_id: Uuid::parse_str(chat_id).unwrap(),
        uploaded_by_user_id: TestUser::A1.user_id,
        filename: "a.pdf".to_owned(),
        content_type: Some("application/pdf".to_owned()),
        size_bytes: Some(10),
        storage_backend: "openai".to_owned(),
        provider_file_id: Some("file-x".to_owned()),
        status: "ready".to_owned(),
        error_code: None,
        attachment_kind: "document".to_owned(),
        for_file_search: true,
        for_code_interpreter: false,
        doc_summary: None,
        img_thumbnail: None,
        img_thumbnail_width: None,
        img_thumbnail_height: None,
        summary_model: None,
        summary_updated_at: None,
        cleanup_status: cleanup_status.map(str::to_owned),
        cleanup_attempts: 0,
        last_cleanup_error: None,
        cleanup_updated_at: None,
        created_at: now,
        updated_at: now,
        deleted_at: deleted.then_some(now),
        secondary_file_id: None,
        secondary_status: "not_attempted".to_owned(),
        secondary_provider_kind: None,
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<attachment::Entity>(row.into_active_model(), &AccessScope::allow_all(), &conn)
        .await
        .unwrap();
    id
}

async fn attachment_row(app: &TestApp, id: Uuid) -> attachment::Model {
    let conn = app.db.conn().unwrap();
    attachment::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("attachment row")
}

#[tokio::test]
async fn delete_chat_hands_live_attachments_to_cleanup() {
    let app = app().await;
    let id = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let other = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let live = seed_attachment(&app, &id, false, None).await;
    let deleted = seed_attachment(&app, &id, true, Some("pending")).await;
    let done = seed_attachment(&app, &id, false, Some("done")).await;
    let other_chat = seed_attachment(&app, &other, false, None).await;

    let r = app
        .call(TestUser::A1, Method::DELETE, &chat_path(&id), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    let chat_deleted_at = chat_row(&app, &id).await.deleted_at.unwrap();

    let a = attachment_row(&app, live).await;
    assert_eq!(a.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(a.cleanup_updated_at, Some(chat_deleted_at));
    assert!(a.deleted_at.is_none());
    assert!(
        attachment_row(&app, deleted)
            .await
            .cleanup_updated_at
            .is_none()
    );
    let d = attachment_row(&app, done).await;
    assert_eq!(d.cleanup_status.as_deref(), Some("done"));
    assert!(d.cleanup_updated_at.is_none());
    assert!(
        attachment_row(&app, other_chat)
            .await
            .cleanup_status
            .is_none()
    );
}

// ---------------------------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------------------------

fn item_ids(v: &Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(id_of).collect()
}

#[tokio::test]
async fn list_chats_orders_by_updated_at_desc_and_paginates() {
    let app = app().await;
    let first = id_of(&create_ok(&app, TestUser::A1, json!({"title": "1"})).await);
    let second = id_of(&create_ok(&app, TestUser::A1, json!({"title": "2"})).await);
    let third = id_of(&create_ok(&app, TestUser::A1, json!({"title": "3"})).await);

    let r = app.call(TestUser::A1, Method::GET, CHATS, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        item_ids(&r.json),
        vec![third.clone(), second.clone(), first.clone()]
    );
    assert_eq!(r.json["page_info"]["limit"], 20);
    assert_eq!(r.json["items"][0]["message_count"], 0);

    let r = app
        .call(
            TestUser::A1,
            Method::PATCH,
            &chat_path(&first),
            Some(json!({"title": "renamed"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);

    let r = app
        .call(TestUser::A1, Method::GET, &format!("{CHATS}?limit=2"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![first.clone(), third.clone()]);
    assert_eq!(r.json["page_info"]["limit"], 2);
    let cursor = r.json["page_info"]["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_owned();

    let r = app
        .call(
            TestUser::A1,
            Method::GET,
            &format!("{CHATS}?limit=2&cursor={cursor}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![second.clone()]);
    assert!(r.json["page_info"]["next_cursor"].is_null(), "{}", r.json);

    let r = app
        .call(
            TestUser::A1,
            Method::GET,
            &format!("{CHATS}?limit=500"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["page_info"]["limit"], 100);
    assert_eq!(item_ids(&r.json).len(), 3);

    let r = app
        .call(TestUser::A1, Method::GET, &format!("{CHATS}?limit=0"), None)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    assert_eq!(
        r.json["context"]["field_violations"][0]["reason"],
        "INVALID_LIMIT"
    );
    assert_eq!(
        r.json["context"]["resource_type"],
        "gts.cf.core.odata.query.v1~"
    );
}

#[tokio::test]
async fn list_chats_filter_and_orderby() {
    let app = app().await;
    let a = id_of(&create_ok(&app, TestUser::A1, json!({"title": "a"})).await);
    let b = id_of(&create_ok(&app, TestUser::A1, json!({"title": "b"})).await);
    let c = id_of(&create_ok(&app, TestUser::A1, json!({"title": "c-b"})).await);

    let get = |q: &'static str| {
        let app = &app;
        async move {
            app.call(TestUser::A1, Method::GET, &format!("{CHATS}?{q}"), None)
                .await
        }
    };

    let r = get("$filter=title%20eq%20'b'").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![b.clone()]);

    let r = get("$filter=contains(title,'b')&$orderby=title%20asc").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![b.clone(), c.clone()]);

    let r = get("$filter=startswith(title,'c')").await;
    assert_eq!(item_ids(&r.json), vec![c.clone()]);
    let r = get("$filter=endswith(title,'b')&$orderby=title%20desc").await;
    assert_eq!(item_ids(&r.json), vec![c.clone(), b.clone()]);

    let r = get("$orderby=title%20asc").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![a.clone(), b.clone(), c.clone()]);

    let r = get("$orderby=title%20asc&limit=1").await;
    let cursor = r.json["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = app
        .call(
            TestUser::A1,
            Method::GET,
            &format!("{CHATS}?limit=1&cursor={cursor}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![b.clone()]);

    let r = app
        .call(
            TestUser::A1,
            Method::GET,
            &format!("{CHATS}?$filter=id%20eq%20{a}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json), vec![a.clone()]);

    let r = get("$filter=updated_at%20gt%202000-01-01T00:00:00Z").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(item_ids(&r.json).len(), 3);

    let r = get("$filter=owner%20eq%20'x'").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    assert_eq!(
        r.json["context"]["field_violations"][0]["reason"],
        "INVALID_FILTER"
    );
    assert_eq!(
        r.json["context"]["resource_type"],
        "gts.cf.core.odata.query.v1~"
    );

    let r = get("$orderby=model%20asc").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    assert_eq!(
        r.json["context"]["field_violations"][0]["reason"],
        "INVALID_ORDERBY_FIELD"
    );

    let r = get("cursor=garbage").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    assert_eq!(
        r.json["context"]["field_violations"][0]["reason"],
        "INVALID_CURSOR"
    );

    let r = get("$skip=1").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    assert_eq!(
        r.json["context"]["field_violations"][0]["reason"],
        "UNSUPPORTED_QUERY_PARAM"
    );

    let r = get("$select=id").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let first = &r.json["items"][0];
    for key in [
        "id",
        "model",
        "is_temporary",
        "message_count",
        "created_at",
        "updated_at",
        "title",
    ] {
        assert!(first.get(key).is_some(), "missing {key}: {first}");
    }
}

#[tokio::test]
async fn list_chats_only_own_and_not_deleted() {
    let app = app().await;
    let kept = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let gone = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let other_user = id_of(&create_ok(&app, TestUser::A2, json!({})).await);
    let other_tenant = id_of(&create_ok(&app, TestUser::B1, json!({})).await);

    let r = app
        .call(TestUser::A1, Method::DELETE, &chat_path(&gone), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);

    let r = app.call(TestUser::A1, Method::GET, CHATS, None).await;
    assert_eq!(item_ids(&r.json), vec![kept]);
    let r = app.call(TestUser::A2, Method::GET, CHATS, None).await;
    assert_eq!(item_ids(&r.json), vec![other_user]);
    let r = app.call(TestUser::B1, Method::GET, CHATS, None).await;
    assert_eq!(item_ids(&r.json), vec![other_tenant]);
}

// ---------------------------------------------------------------------------------------------
// Authorization failures
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn pdp_deny_is_403_and_pdp_failure_is_503() {
    let some_id = Uuid::new_v4().to_string();

    let denied = TestApp::builder().pdp(PdpMode::Deny).build().await;
    for (method, path, body) in [
        (Method::POST, CHATS.to_owned(), Some(json!({}))),
        (Method::GET, CHATS.to_owned(), None),
        (Method::GET, chat_path(&some_id), None),
        (
            Method::PATCH,
            chat_path(&some_id),
            Some(json!({"title": "x"})),
        ),
        (Method::DELETE, chat_path(&some_id), None),
    ] {
        let r = denied.call(TestUser::A1, method.clone(), &path, body).await;
        assert_eq!(
            r.status,
            StatusCode::FORBIDDEN,
            "{method} {path}: {}",
            r.json
        );
        assert_eq!(r.json["context"]["reason"], "AUTHZ_DENIED", "{}", r.json);
    }

    let failing = TestApp::builder().pdp(PdpMode::Fail).build().await;
    for (method, path, body) in [
        (Method::POST, CHATS.to_owned(), Some(json!({}))),
        (Method::GET, CHATS.to_owned(), None),
        (Method::GET, chat_path(&some_id), None),
    ] {
        let r = failing
            .call(TestUser::A1, method.clone(), &path, body)
            .await;
        assert_eq!(
            r.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path}: {}",
            r.json
        );
        assert_eq!(
            r.headers.get("retry-after").unwrap().to_str().unwrap(),
            "5",
            "{}",
            r.json
        );
    }
}

/// Follows `next_cursor` from `first_query` with `limit=1`; returns every item id.
async fn walk_pages(app: &TestApp, first_query: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut url = format!("{CHATS}?limit=1&{first_query}");
    for _ in 0..20 {
        let r = app.call(TestUser::A1, Method::GET, &url, None).await;
        assert_eq!(r.status, StatusCode::OK, "{url}: {}", r.json);
        ids.extend(item_ids(&r.json));
        match r.json["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{CHATS}?limit=1&cursor={c}"),
            None => return ids,
        }
    }
    panic!("pagination did not terminate: {ids:?}");
}

#[tokio::test]
async fn updated_at_eq_filter_matches_exact_value() {
    let app = app().await;
    let _other = create_ok(&app, TestUser::A1, json!({})).await;
    let v = create_ok(&app, TestUser::A1, json!({"title": "t"})).await;
    let updated_at = v["updated_at"].as_str().unwrap();
    for op in ["eq", "ge", "le"] {
        let r = app
            .call(
                TestUser::A1,
                Method::GET,
                &format!("{CHATS}?$filter=updated_at%20{op}%20{updated_at}"),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        assert!(item_ids(&r.json).contains(&id_of(&v)), "{op}: {}", r.json);
    }
    let r = app
        .call(
            TestUser::A1,
            Method::GET,
            &format!("{CHATS}?$filter=updated_at%20eq%20{updated_at}"),
            None,
        )
        .await;
    assert_eq!(item_ids(&r.json), vec![id_of(&v)]);
}

#[tokio::test]
async fn paging_does_not_skip_rows_sharing_updated_at() {
    let app = app().await;
    let mut tied = Vec::new();
    for _ in 0..3 {
        tied.push(id_of(&create_ok(&app, TestUser::A1, json!({})).await));
    }
    let newest = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let same = chat_row(&app, &tied[0]).await.updated_at;
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::value(same))
        .filter(chat::Column::Id.is_in(tied.iter().map(|i| Uuid::parse_str(i).unwrap())))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&app.db.conn().unwrap())
        .await
        .unwrap();

    let ids = walk_pages(&app, "").await;
    assert_eq!(ids.len(), 4, "{ids:?}");
    assert_eq!(ids[0], newest);
    let mut tied_desc = tied.clone();
    tied_desc.sort_by(|a, b| b.cmp(a)); // tiebreaker `id desc`
    assert_eq!(ids[1..], tied_desc[..]);
}

#[tokio::test]
async fn title_ordering_pages_through_untitled_chats() {
    let app = app().await;
    let b = id_of(&create_ok(&app, TestUser::A1, json!({"title": "b"})).await);
    let u1 = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    let a = id_of(&create_ok(&app, TestUser::A1, json!({"title": "a"})).await);
    let u2 = id_of(&create_ok(&app, TestUser::A1, json!({})).await);
    assert!(
        chat_row(&app, &u1).await.title.is_none(),
        "untitled stays NULL"
    );

    let mut untitled = [u1, u2];
    untitled.sort_by(|x, y| y.cmp(x)); // tiebreaker `id desc`

    let asc = walk_pages(&app, "$orderby=title%20asc").await;
    let mut want = untitled.to_vec();
    want.extend([a.clone(), b.clone()]);
    assert_eq!(asc, want);

    let desc = walk_pages(&app, "$orderby=title%20desc").await;
    let mut want = vec![b, a];
    want.extend(untitled);
    assert_eq!(desc, want);
}
