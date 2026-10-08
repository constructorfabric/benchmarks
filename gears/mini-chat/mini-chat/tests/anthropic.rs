//! Anthropic chats: the secondary image copy in the Anthropic Files API
//! (DESIGN §2.2 "File storage (P1)", §3.7 `attachments.secondary_*`, §4
//! "Attachment Deletion" `secondary_ref`) and the Messages adapter end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::{Method, StatusCode};
use mini_chat::infra::db::entities::attachment;
use mini_chat::infra::outbox::QueueKind;
use mini_chat::testing::catalog::{VISION_INPUT, premium_model};
use mini_chat::testing::images::png;
use mini_chat::testing::providers::anthropic_entry;
use mini_chat::testing::{FakeFile, ScriptedStream, TestApp, TestUser};
use sea_orm::EntityTrait;
use serde_json::{Value, json};
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;
const ANTHROPIC: &str = "api.anthropic.com";
const OPENAI: &str = "api.openai.com";

fn claude() -> mini_chat_sdk::ModelCatalogEntry {
    let mut m = premium_model("claude");
    m.provider_id = "anthropic".to_owned();
    m.multimodal_capabilities = vec![VISION_INPUT.to_owned()];
    m
}

async fn app_with(
    f: impl FnOnce(&mut mini_chat::config::MiniChatConfig) + Send + 'static,
) -> TestApp {
    TestApp::builder()
        .catalog(vec![claude()])
        .config(|c| {
            c.providers
                .insert("anthropic".to_owned(), anthropic_entry());
            f(c);
        })
        .build()
        .await
}

async fn create_chat(app: &TestApp) -> Uuid {
    let r = app.call(U, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn upload(app: &TestApp, chat: Uuid, name: &str, ct: &str, data: &[u8]) -> Uuid {
    let r = app.upload(U, chat, name, ct, data).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "ready", "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

async fn row(app: &TestApp, id: Uuid) -> attachment::Model {
    let conn = app.db.conn().unwrap();
    attachment::Entity::find_by_id(id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .expect("attachment row")
}

fn files_at(app: &TestApp, alias: &str) -> Vec<FakeFile> {
    app.provider
        .files()
        .into_iter()
        .filter(|f| f.alias == alias)
        .collect()
}

/// Waits (up to 5 s) until every uploaded file is deleted.
async fn wait_all_deleted(app: &TestApp) {
    for _ in 0..250 {
        if app.provider.files().iter().all(|f| f.deleted) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("files not deleted: {:?}", app.provider.files());
}

fn header<'a>(req: &'a mini_chat::testing::RecordedRequest, name: &str) -> Option<&'a str> {
    req.headers.get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn anthropic_chat_image_gets_secondary_copy() {
    let app = app_with(|_| {}).await;
    let chat = create_chat(&app).await;
    let image = png(4, 4);
    let id = upload(&app, chat, "pic.png", "image/png", &image).await;

    // Primary copy in the RAG provider, secondary copy in the Anthropic Files API.
    let primary = files_at(&app, OPENAI);
    let secondary = files_at(&app, ANTHROPIC);
    assert_eq!(primary.len(), 1);
    assert_eq!(secondary.len(), 1);
    assert_eq!(secondary[0].bytes.as_ref(), image.as_slice());
    assert_eq!(secondary[0].filename, primary[0].filename);
    // Only the `file` part, no `purpose`.
    assert_eq!(secondary[0].purpose, "");
    let upload_req = app
        .provider
        .requests()
        .into_iter()
        .find(|r| r.path == format!("/{ANTHROPIC}/v1/files"))
        .unwrap();
    assert_eq!(header(&upload_req, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(
        header(&upload_req, "anthropic-beta"),
        Some("files-api-2025-04-14")
    );

    let a = row(&app, id).await;
    assert_eq!(a.status, "ready");
    assert_eq!(a.provider_file_id.as_deref(), Some(primary[0].id.as_str()));
    assert_eq!(a.secondary_status, "uploaded");
    assert_eq!(a.secondary_provider_kind.as_deref(), Some("anthropic"));
    assert_eq!(
        a.secondary_file_id.as_deref(),
        Some(secondary[0].id.as_str())
    );

    // Attachment deletion hands both files to the cleanup via `secondary_ref`.
    let r = app
        .call(
            U,
            Method::DELETE,
            &format!("{CHATS}/{chat}/attachments/{id}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    let msgs = app.outbox_messages(QueueKind::AttachmentCleanup).await;
    assert_eq!(
        msgs[0]["secondary_ref"],
        json!({"file_id": secondary[0].id, "provider_kind": "anthropic", "upstream_alias": ANTHROPIC})
    );
    wait_all_deleted(&app).await;
    let delete_req = app
        .provider
        .requests()
        .into_iter()
        .find(|r| r.method == Method::DELETE && r.path.starts_with(&format!("/{ANTHROPIC}/")))
        .unwrap();
    assert_eq!(
        delete_req.path,
        format!("/{ANTHROPIC}/v1/files/{}", secondary[0].id)
    );
    assert_eq!(header(&delete_req, "anthropic-version"), Some("2023-06-01"));
    assert_eq!(row(&app, id).await.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn documents_and_oversized_images_get_no_secondary_copy() {
    let image = png(16, 16);
    let limit = image.len() - 1;
    let app = app_with(move |c| c.thumbnail.max_decode_bytes = limit).await;
    let chat = create_chat(&app).await;

    let doc = upload(&app, chat, "a.pdf", "application/pdf", b"%PDF-1.4\n% d\n").await;
    let big = upload(&app, chat, "big.png", "image/png", &image).await;
    assert!(files_at(&app, ANTHROPIC).is_empty());
    for id in [doc, big] {
        let a = row(&app, id).await;
        assert_eq!(a.secondary_status, "not_attempted");
        assert_eq!(a.secondary_file_id, None);
        assert_eq!(a.secondary_provider_kind, None);
    }
}

#[tokio::test]
async fn failed_secondary_upload_keeps_the_attachment_usable() {
    let app = app_with(|_| {}).await;
    let chat = create_chat(&app).await;
    app.provider
        .fail_next(&format!("/{ANTHROPIC}/v1/files"), 500);
    let id = upload(&app, chat, "pic.png", "image/png", &png(4, 4)).await;
    let a = row(&app, id).await;
    assert_eq!(a.status, "ready");
    assert_eq!(a.secondary_status, "failed");
    assert_eq!(a.secondary_file_id, None);
}

#[tokio::test]
async fn chat_cleanup_deletes_secondary_copy() {
    let app = app_with(|_| {}).await;
    let chat = create_chat(&app).await;
    upload(&app, chat, "pic.png", "image/png", &png(4, 4)).await;
    let r = app
        .call(U, Method::DELETE, &format!("{CHATS}/{chat}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    wait_all_deleted(&app).await;
    assert_eq!(files_at(&app, ANTHROPIC).len(), 1);
}

fn anthropic_text(text: &str) -> ScriptedStream {
    let ev = |name: &str, data: Value| {
        let mut data = data;
        data["type"] = json!(name);
        (name.to_owned(), data)
    };
    ScriptedStream::events(vec![
        ev(
            "message_start",
            json!({"message": {"id": "msg_1", "usage": {"input_tokens": 9, "output_tokens": 1}}}),
        ),
        ev(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        ev(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": text}}),
        ),
        ev("content_block_stop", json!({"index": 0})),
        ev(
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 4}}),
        ),
        ev("message_stop", json!({})),
    ])
}

#[tokio::test]
async fn anthropic_chat_sends_secondary_image_ids() {
    let app = app_with(|_| {}).await;
    let chat = create_chat(&app).await;
    let id = upload(&app, chat, "pic.png", "image/png", &png(4, 4)).await;
    let secondary = row(&app, id).await.secondary_file_id.unwrap();
    app.provider.push_stream(anthropic_text("A cat."));

    let sse = app
        .stream(
            U,
            &format!("{CHATS}/{chat}/messages:stream"),
            json!({"content": "what is it?", "attachment_ids": [id]}),
        )
        .await;
    assert_eq!(sse.names(), ["stream_started", "delta", "done"]);
    assert_eq!(sse.first("delta").unwrap()["content"], "A cat.");
    assert_eq!(
        sse.first("done").unwrap()["usage"],
        json!({"input_tokens": 9, "output_tokens": 4})
    );

    let req = app
        .provider
        .requests()
        .into_iter()
        .find(|r| r.path == format!("/{ANTHROPIC}/v1/messages"))
        .unwrap();
    assert_eq!(header(&req, "anthropic-version"), Some("2023-06-01"));
    let body = req.json.unwrap();
    assert_eq!(body["model"], "claude");
    let last = body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"],
        json!([
            {"type": "text", "text": "what is it?"},
            {"type": "image", "source": {"type": "file", "file_id": secondary}},
        ])
    );
}

/// Two Anthropic entries: `anthropic` (lowest id, `api.anthropic.com`) and
/// `anthropic-b` (`b.anthropic.example`), which serves the chat's model.
async fn two_anthropic_app() -> TestApp {
    let mut model = claude();
    model.provider_id = "anthropic-b".to_owned();
    TestApp::builder()
        .catalog(vec![model])
        .config(|c| {
            c.providers
                .insert("anthropic".to_owned(), anthropic_entry());
            let mut b = anthropic_entry();
            "b.anthropic.example".clone_into(&mut b.host);
            c.providers.insert("anthropic-b".to_owned(), b);
        })
        .build()
        .await
}

fn deletes_at(app: &TestApp, alias: &str) -> Vec<String> {
    app.provider
        .requests()
        .into_iter()
        .filter(|r| r.method == Method::DELETE && r.path.starts_with(&format!("/{alias}/")))
        .map(|r| r.path)
        .collect()
}

#[tokio::test]
async fn secondary_copy_is_deleted_on_the_chat_models_anthropic_provider() {
    const B: &str = "b.anthropic.example";
    let app = two_anthropic_app().await;
    let chat = create_chat(&app).await;
    let id = upload(&app, chat, "pic.png", "image/png", &png(4, 4)).await;
    let secondary = files_at(&app, B);
    assert_eq!(secondary.len(), 1, "{:?}", app.provider.files());
    assert!(files_at(&app, ANTHROPIC).is_empty());

    let r = app
        .call(
            U,
            Method::DELETE,
            &format!("{CHATS}/{chat}/attachments/{id}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    let msgs = app.outbox_messages(QueueKind::AttachmentCleanup).await;
    assert_eq!(msgs[0]["secondary_ref"]["upstream_alias"], B);
    wait_all_deleted(&app).await;
    assert_eq!(
        deletes_at(&app, B),
        [format!("/{B}/v1/files/{}", secondary[0].id)]
    );
    assert!(deletes_at(&app, ANTHROPIC).is_empty());
}

#[tokio::test]
async fn chat_cleanup_deletes_secondary_copy_on_the_chat_models_provider() {
    const B: &str = "b.anthropic.example";
    let app = two_anthropic_app().await;
    let chat = create_chat(&app).await;
    upload(&app, chat, "pic.png", "image/png", &png(4, 4)).await;
    let secondary = files_at(&app, B);
    let r = app
        .call(U, Method::DELETE, &format!("{CHATS}/{chat}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    wait_all_deleted(&app).await;
    assert_eq!(
        deletes_at(&app, B),
        [format!("/{B}/v1/files/{}", secondary[0].id)]
    );
    assert!(deletes_at(&app, ANTHROPIC).is_empty());
}
