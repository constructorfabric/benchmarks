#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Attachments in the send pipeline: attachment validation inside the
//! reserve transaction, images as `input_image`, `file_search` /
//! `code_interpreter` tool inclusion, web search tool and the daily tool
//! quotas (S§6.1, D "File Search / Code Interpreter Tool Availability",
//! D "Web Search Quota Enforcement").

mod common;

use axum::http::StatusCode;
use mini_chat::infra::db::entity::{attachment, quota_usage};
use mini_chat::infra::db::repos::{AttachmentRepo, ChatRepo, QuotaUsageRepo};
use mini_chat_sdk::{KillSwitches, ModelPreference, TierLimits};
use serde_json::{Value, json};
use time::{Date, Month};
use uuid::Uuid;

use common::*;

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

/// Date of `CLOCK_START`.
fn today() -> Date {
    Date::from_calendar_date(2025, Month::October, 9).unwrap()
}

async fn seed_daily_total(
    app: &TestApp,
    tenant: Uuid,
    user: Uuid,
    f: impl FnOnce(&mut quota_usage::Model),
) {
    let mut row = quota_row(tenant, user, "total");
    row.period_start = today();
    f(&mut row);
    QuotaUsageRepo
        .insert(&app.db.conn().unwrap(), &tenant_scope(tenant, user), row)
        .await
        .unwrap();
}

/// Send and return the whole response.
async fn send(client: &UserClient<'_>, chat: Uuid, body: &Value) -> TestResponse {
    client.post_json(&stream_path(chat), body).await
}

/// Send, expect a completed SSE turn, return the recorded provider body.
async fn send_ok(app: &TestApp, client: &UserClient<'_>, chat: Uuid, body: &Value) -> Value {
    push_hello(app);
    let resp = send(client, chat, body).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let events = resp.sse_events();
    assert_eq!(events.last().unwrap().0, "done", "{events:?}");
    last_provider_body(app)
}

fn last_provider_body(app: &TestApp) -> Value {
    app.oagw
        .requests()
        .into_iter()
        .rfind(|r| r.uri.contains("/responses"))
        .and_then(|r| r.json_body)
        .expect("a provider request")
}

fn tool_types(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .map(|x| x["type"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn reason_of(body: &Value) -> String {
    body["context"]["field_violations"][0]["reason"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------

#[tokio::test]
async fn image_in_message_sent_as_input_image() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let img = upload_image_ready(&app, &client, chat, "file-pic1").await;

    let body = send_ok(
        &app,
        &client,
        chat,
        &json!({"content": "what is in the picture?", "attachment_ids": [img]}),
    )
    .await;

    let input = body["input"].as_array().unwrap();
    let current = &input.last().unwrap()["content"];
    let parts = current.as_array().expect("multimodal content parts");
    assert!(
        parts.contains(&json!({"type": "input_image", "file_id": "file-pic1"})),
        "{current}"
    );
    assert!(tool_types(&body).is_empty(), "{body}");

    let msgs = client.get(&messages_path(chat)).await.json();
    let user_msg = &msgs["items"][0];
    assert_eq!(user_msg["role"], "user");
    let att = &user_msg["attachments"][0];
    assert_eq!(att["attachment_id"], img.to_string());
    assert_eq!(att["kind"], "image");
    assert_eq!(att["status"], "ready");
    assert_eq!(att["filename"], "pic.png");
    assert_eq!(att["img_thumbnail"]["content_type"], "image/webp");
    assert_eq!(att["img_thumbnail"]["width"], 128);
    assert_eq!(msgs["items"][1]["attachments"], json!([]));
}

#[tokio::test]
async fn too_many_images() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let mut imgs = Vec::new();
    for i in 0..5 {
        imgs.push(upload_image_ready(&app, &client, chat, &format!("file-i{i}")).await);
    }

    let resp = send(
        &client,
        chat,
        &json!({"content": "look", "attachment_ids": imgs}),
    )
    .await;

    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    assert_eq!(reason_of(&resp.json()), "TOO_MANY_IMAGES");
    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());

    // Four are fine.
    send_ok(
        &app,
        &client,
        chat,
        &json!({"content": "look", "attachment_ids": imgs[..4]}),
    )
    .await;
}

#[tokio::test]
async fn vision_not_supported_after_downgrade() {
    let mut fallback = standard_no_vision("s-novision");
    fallback.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let (std_l, prem_l) = (
        TierLimits {
            limit_daily_credits_micro: 1_000_000_000_000,
            limit_monthly_credits_micro: 30_000_000_000_000,
        },
        TierLimits {
            limit_daily_credits_micro: 1_000_000,
            limit_monthly_credits_micro: 30_000_000,
        },
    );
    let app = TestApp::builder()
        .catalog(vec![premium_model("p1"), fallback])
        .limits(std_l, prem_l)
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "p1").await;
    let img = upload_image_ready(&app, &client, chat, "file-v").await;
    let mut row = quota_row(tenant, user, "tier:premium");
    row.period_start = today();
    row.spent_credits_micro = 1_000_000;
    QuotaUsageRepo
        .insert(&app.db.conn().unwrap(), &tenant_scope(tenant, user), row)
        .await
        .unwrap();

    let resp = send(
        &client,
        chat,
        &json!({"content": "see", "attachment_ids": [img]}),
    )
    .await;

    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    assert_eq!(reason_of(&resp.json()), "VISION_NOT_SUPPORTED");
    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
}

#[tokio::test]
async fn images_kill_switch_on_message() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let img = upload_image_ready(&app, &client, chat, "file-k").await;
    app.policy.set_kill_switches(KillSwitches {
        disable_images: true,
        ..KillSwitches::default()
    });

    let resp = send(
        &client,
        chat,
        &json!({"content": "see", "attachment_ids": [img]}),
    )
    .await;

    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    let v = &resp.json()["context"]["violations"][0];
    assert_eq!(v["subject"], "images");
    assert_eq!(v["type"], "FEATURE_DISABLED");
    assert_eq!(provider_calls(&app), 0);
}

// ---------------------------------------------------------------------------
// Attachment validation in the reserve transaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attachment_validation_in_reserve_txn() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = ChatRepo
        .find_by_id(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            chat_id,
        )
        .await
        .unwrap()
        .unwrap();
    let conn = app.db.conn().unwrap();
    let scope = tenant_scope(tenant, user);
    let insert = |row: attachment::Model| {
        let (conn, scope) = (&conn, &scope);
        async move {
            let id = row.id;
            AttachmentRepo.insert(conn, scope, row).await.unwrap();
            id
        }
    };
    let foreign = insert(attachment::Model {
        uploaded_by_user_id: Uuid::new_v4(),
        status: "ready".to_owned(),
        provider_file_id: Some("file-foreign".to_owned()),
        ..attachment_row(&chat)
    })
    .await;
    let not_ready = insert(attachment::Model {
        status: "uploaded".to_owned(),
        provider_file_id: Some("file-pending".to_owned()),
        ..attachment_row(&chat)
    })
    .await;
    let deleted = insert(attachment::Model {
        status: "ready".to_owned(),
        deleted_at: Some(chat.created_at),
        ..attachment_row(&chat)
    })
    .await;
    let other_chat = create_chat(&client, "s1").await;
    let elsewhere = upload_image_ready(&app, &client, other_chat, "file-else").await;
    let provider_before = provider_calls(&app);

    for bad in [foreign, not_ready, deleted, elsewhere, Uuid::new_v4()] {
        let resp = send(
            &client,
            chat_id,
            &json!({"content": "x", "attachment_ids": [bad]}),
        )
        .await;
        assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
        assert_eq!(reason_of(&resp.json()), "invalid_attachment");
    }

    assert_eq!(provider_calls(&app), provider_before);
    assert!(all_turns(&app).await.is_empty());
    assert!(all_messages(&app).await.is_empty());
    for row in quota_rows(&app).await {
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
    }
}

// ---------------------------------------------------------------------------
// file_search / code_interpreter
// ---------------------------------------------------------------------------

#[tokio::test]
async fn file_search_tool_only_after_ready_document() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let guard = app.config.context.file_search_guard.clone();

    let body = send_ok(&app, &client, chat, &json!({"content": "one"})).await;
    assert!(tool_types(&body).is_empty(), "{body}");
    assert!(!body["instructions"].as_str().unwrap().contains(&guard));

    // A document still indexing does not enable the tool.
    script_file(&app, "file-idx");
    script_vs_create(&app, "vs_fs");
    script_add(&app, "vs_fs", "file-idx", "in_progress");
    status_fallback(&app, "vs_fs", "file-idx", "in_progress");
    let uploaded = created(&upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await);
    assert_eq!(uploaded["status"], "uploaded");
    let body = send_ok(&app, &client, chat, &json!({"content": "two"})).await;
    assert!(tool_types(&body).is_empty(), "{body}");

    let doc = upload_doc_ready(&app, &client, chat, "file-ready", "vs_fs", false).await;
    let body = send_ok(&app, &client, chat, &json!({"content": "three"})).await;
    assert_eq!(tool_types(&body), ["file_search"]);
    assert_eq!(body["tools"][0]["vector_store_ids"], json!(["vs_fs"]));
    assert_eq!(body["tools"][0]["max_num_results"], 5);
    assert!(
        body["instructions"].as_str().unwrap().contains(&guard),
        "{body}"
    );

    // Deleting the only ready document removes the tool.
    let resp = client.delete(&attachment_path(chat, doc)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    let body = send_ok(&app, &client, chat, &json!({"content": "four"})).await;
    assert!(tool_types(&body).is_empty(), "{body}");
}

#[tokio::test]
async fn code_interpreter_tool_with_ready_xlsx() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    upload_xlsx_ready(&app, &client, chat, "file-sheet").await;

    let body = send_ok(&app, &client, chat, &json!({"content": "sum column A"})).await;

    assert_eq!(tool_types(&body), ["code_interpreter"]);
    assert_eq!(
        body["tools"][0]["container"],
        json!({"type": "auto", "file_ids": ["file-sheet"]})
    );
    assert_eq!(body["include"], json!(["code_interpreter_call.outputs"]));

    app.policy.set_kill_switches(KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    });
    let body = send_ok(&app, &client, chat, &json!({"content": "again"})).await;
    assert!(tool_types(&body).is_empty(), "{body}");
    assert!(body.get("include").is_none(), "{body}");
}

#[tokio::test]
async fn code_interpreter_daily_quota_429() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    upload_xlsx_ready(&app, &client, chat, "file-q").await;
    seed_daily_total(&app, tenant, user, |r| r.code_interpreter_calls = 50).await;

    let resp = send(&client, chat, &json!({"content": "compute"})).await;

    assert_eq!(
        resp.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        resp.text()
    );
    assert_eq!(
        resp.json()["context"]["violations"][0]["subject"],
        "code_interpreter"
    );
    assert_eq!(provider_calls(&app), 0);
    assert!(all_turns(&app).await.is_empty());
}

// ---------------------------------------------------------------------------
// Web search
// ---------------------------------------------------------------------------

#[tokio::test]
async fn web_search_tool_and_daily_quota() {
    let mut no_ws = standard_model("s-nows");
    no_ws.general_config.tool_support.web_search = false;
    no_ws.preference = None;
    let mut catalog = vec![premium_model("p1"), standard_model("s1")];
    catalog.push(no_ws);
    let app = TestApp::builder().catalog(catalog).build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let ws_on = json!({"content": "news?", "web_search": {"enabled": true}});

    let body = send_ok(&app, &client, chat, &ws_on).await;
    assert_eq!(tool_types(&body), ["web_search"]);
    let instructions = body["instructions"].as_str().unwrap();
    assert!(
        instructions.contains(app.config.context.web_search_guard.as_str()),
        "{instructions}"
    );

    // The first turn created the daily row.
    raw_exec(
        &app.raw,
        "UPDATE quota_usage SET web_search_calls = 75 WHERE period_type = 'daily' AND bucket = 'total'",
    )
    .await
    .unwrap();
    let resp = send(&client, chat, &ws_on).await;
    assert_eq!(
        resp.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        resp.text()
    );
    assert_eq!(
        resp.json()["context"]["violations"][0]["subject"],
        "web_search"
    );
    // A request without web search is not affected by the web search quota.
    send_ok(&app, &client, chat, &json!({"content": "plain"})).await;

    // A model without web search: no tool and no web search quota check.
    let chat2 = create_chat(&client, "s-nows").await;
    let request_id = Uuid::new_v4();
    let mut body = ws_on.clone();
    body["request_id"] = json!(request_id);
    let sent = send_ok(&app, &client, chat2, &body).await;
    assert!(tool_types(&sent).is_empty(), "{sent}");
    assert!(turn_by_request(&app, request_id).await.web_search_enabled);
}

#[tokio::test]
async fn web_citations_source_web() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw.push_sse(
        PROVIDER_PATH,
        vec![
            tool_start("web_search"),
            (
                "response.web_search_call.completed",
                json!({"type": "response.web_search_call.completed"}),
            ),
            delta("Rust 2024 is out"),
            (
                "response.output_text.annotation.added",
                json!({"type": "response.output_text.annotation.added", "output_index": 0,
                       "content_index": 0, "annotation": {"type": "url_citation",
                       "url": "https://blog.rust-lang.org/", "title": "Rust Blog",
                       "start_index": 0, "end_index": 4}}),
            ),
            completed(20, 5),
        ],
    );

    let resp = send(
        &client,
        chat,
        &json!({"content": "rust news", "web_search": {"enabled": true}}),
    )
    .await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let events = resp.sse_events();
    let citations = event(&events, "citations");
    assert_eq!(
        citations["items"],
        json!([{"source": "web", "title": "Rust Blog", "url": "https://blog.rust-lang.org/",
                "snippet": "Rust", "span": {"start": 0, "end": 4}}])
    );
}

#[tokio::test]
async fn web_search_calls_counted_in_quota_usage() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let done = || {
        (
            "response.web_search_call.completed",
            json!({"type": "response.web_search_call.completed"}),
        )
    };
    app.oagw.push_sse(
        PROVIDER_PATH,
        vec![
            tool_start("web_search"),
            done(),
            tool_start("web_search"),
            done(),
            delta("answer"),
            completed(20, 5),
        ],
    );

    let resp = send(
        &client,
        chat,
        &json!({"content": "search", "web_search": {"enabled": true}}),
    )
    .await;

    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.sse_events().last().unwrap().0, "done");
    assert_eq!(
        quota_row_of(&app, "daily", "total").await.web_search_calls,
        2
    );
    assert_eq!(
        quota_row_of(&app, "monthly", "total")
            .await
            .web_search_calls,
        2
    );
}
