//! Helpers for the attachment tests: paths, fixture files, scripted storage
//! replies (`OpenAI` Files / Vector Stores through `FakeOagw`) and unscoped
//! DB reads.

#![allow(
    clippy::disallowed_methods,
    reason = "tests read rows unscoped through the raw connection"
)]

use std::io::Cursor;
use std::time::Duration;

use axum::http::StatusCode;
use image::{ImageFormat, Rgba, RgbaImage};
use sea_orm::{EntityTrait, QueryOrder};
use serde_json::{Value, json};
use uuid::Uuid;

use mini_chat::infra::db::entity::{attachment, chat_vector_store};
use mini_chat::test_support::fake_oagw::RecordedRequest;

use super::app::{MultipartPart, TestApp, TestResponse, UserClient};

/// Files API path of the default `openai` entry.
pub const FILES_PATH: &str = "/v1/files";
/// Vector Stores API path of the default `openai` entry.
pub const VS_PATH: &str = "/v1/vector_stores";

pub const PDF_CT: &str = "application/pdf";
pub const XLSX_CT: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
pub const PDF_BYTES: &[u8] = b"%PDF-1.4 test document body";
pub const XLSX_BYTES: &[u8] = b"PK\x03\x04 fake spreadsheet";

pub fn attachments_path(chat: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments")
}

pub fn attachment_path(chat: Uuid, id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments/{id}")
}

/// A `w` x `h` PNG.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_fn(w, h, |x, y| {
        let byte = |v: u32| u8::try_from(v % 256).unwrap();
        Rgba([byte(x), byte(y), 128, 255])
    });
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).unwrap();
    out.into_inner()
}

/// `POST /attachments` with one `file` part.
pub async fn upload(
    client: &UserClient<'_>,
    chat: Uuid,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
) -> TestResponse {
    client
        .post_multipart(
            &attachments_path(chat),
            vec![MultipartPart::file("file", filename, content_type, bytes)],
        )
        .await
}

/// Files API upload reply `{"id": file_id}`.
pub fn script_file(app: &TestApp, file_id: &str) {
    app.oagw.push_json(
        FILES_PATH,
        200,
        json!({"id": file_id, "object": "file", "purpose": "assistants"}),
    );
}

/// Vector store creation reply `{"id": vs}`.
pub fn script_vs_create(app: &TestApp, vs: &str) {
    app.oagw
        .push_json(VS_PATH, 200, json!({"id": vs, "object": "vector_store"}));
}

/// `POST /vector_stores/{vs}/files` reply with `status` (POST only, so a
/// background status poll of another file cannot take it).
pub fn script_add(app: &TestApp, vs: &str, file_id: &str, status: &str) {
    app.oagw.push_json_for(
        "POST",
        &format!("{VS_PATH}/{vs}/files"),
        200,
        json!({"id": file_id, "object": "vector_store.file", "status": status}),
    );
}

/// Path of `GET /vector_stores/{vs}/files/{file}`.
pub fn status_path(vs: &str, file_id: &str) -> String {
    format!("{VS_PATH}/{vs}/files/{file_id}")
}

/// One `GET /vector_stores/{vs}/files/{file}` reply with `status`.
pub fn script_status(app: &TestApp, vs: &str, file_id: &str, status: &str) {
    app.oagw.push_json(
        &status_path(vs, file_id),
        200,
        json!({"id": file_id, "object": "vector_store.file", "status": status}),
    );
}

/// Every `GET /vector_stores/{vs}/files/{file}` without a one-shot reply
/// returns `status`.
pub fn status_fallback(app: &TestApp, vs: &str, file_id: &str, status: &str) {
    app.oagw.set_fallback_json(
        &status_path(vs, file_id),
        200,
        json!({"id": file_id, "object": "vector_store.file", "status": status}),
    );
}

/// 201 body of a successful upload.
pub fn created(resp: &TestResponse) -> Value {
    assert_eq!(resp.status, StatusCode::CREATED, "{}", resp.text());
    resp.json()
}

fn id_of(body: &Value) -> Uuid {
    body["id"].as_str().unwrap().parse().unwrap()
}

/// Upload a PDF that is indexed at once. `create_vs` scripts the creation of
/// the chat's vector store (first document of the chat). Returns its id.
pub async fn upload_doc_ready(
    app: &TestApp,
    client: &UserClient<'_>,
    chat: Uuid,
    file_id: &str,
    vs: &str,
    create_vs: bool,
) -> Uuid {
    script_file(app, file_id);
    if create_vs {
        script_vs_create(app, vs);
    }
    script_add(app, vs, file_id, "completed");
    let body = created(&upload(client, chat, "doc.pdf", PDF_CT, PDF_BYTES).await);
    assert_eq!(body["status"], "ready", "{body}");
    id_of(&body)
}

/// Upload a 300x200 PNG (ready with thumbnail). Returns its id.
pub async fn upload_image_ready(
    app: &TestApp,
    client: &UserClient<'_>,
    chat: Uuid,
    file_id: &str,
) -> Uuid {
    script_file(app, file_id);
    let body = created(&upload(client, chat, "pic.png", "image/png", &png(300, 200)).await);
    assert_eq!(body["status"], "ready", "{body}");
    id_of(&body)
}

/// Upload an XLSX (ready for code interpreter). Returns its id.
pub async fn upload_xlsx_ready(
    app: &TestApp,
    client: &UserClient<'_>,
    chat: Uuid,
    file_id: &str,
) -> Uuid {
    script_file(app, file_id);
    let body = created(&upload(client, chat, "sheet.xlsx", XLSX_CT, XLSX_BYTES).await);
    assert_eq!(body["status"], "ready", "{body}");
    id_of(&body)
}

/// Every attachment row (unscoped), oldest first.
pub async fn attachment_rows(app: &TestApp) -> Vec<attachment::Model> {
    attachment::Entity::find()
        .order_by_asc(attachment::Column::CreatedAt)
        .all(&app.raw)
        .await
        .unwrap()
}

/// The attachment row `id` (unscoped).
pub async fn attachment_by_id(app: &TestApp, id: Uuid) -> attachment::Model {
    attachment::Entity::find_by_id(id)
        .one(&app.raw)
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("no attachment {id}"))
}

/// Every `chat_vector_stores` row (unscoped).
pub async fn vector_store_rows(app: &TestApp) -> Vec<chat_vector_store::Model> {
    chat_vector_store::Entity::find()
        .all(&app.raw)
        .await
        .unwrap()
}

/// Recorded proxy requests with `method` whose URI contains `needle`.
pub fn requests_matching(app: &TestApp, method: &str, needle: &str) -> Vec<RecordedRequest> {
    app.oagw
        .requests()
        .into_iter()
        .filter(|r| r.method == method && r.uri.contains(needle))
        .collect()
}

/// Recorded vector store creations (`POST .../vector_stores`).
pub fn vs_creates(app: &TestApp) -> usize {
    app.oagw
        .requests()
        .iter()
        .filter(|r| r.method == "POST" && r.uri.ends_with("/v1/vector_stores"))
        .count()
}

/// Poll `f` until it returns `true` (at most `secs` seconds).
pub async fn wait_until<F, Fut>(secs: u64, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    while tokio::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached within {secs}s");
}

/// Assert a Problem response with `status`; returns its JSON.
pub fn expect_problem(resp: &TestResponse, status: StatusCode) -> Value {
    assert_eq!(resp.status, status, "{}", resp.text());
    assert_eq!(
        resp.header("content-type").as_deref(),
        Some("application/problem+json"),
        "{}",
        resp.text()
    );
    resp.json()
}

/// `(field, reason)` of the first field violation.
pub fn violation(body: &Value) -> (String, String) {
    let v = &body["context"]["field_violations"][0];
    (
        v["field"].as_str().unwrap_or_default().to_owned(),
        v["reason"].as_str().unwrap_or_default().to_owned(),
    )
}
