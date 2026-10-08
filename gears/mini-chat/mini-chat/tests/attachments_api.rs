//! Attachments: `POST /v1/chats/{id}/attachments`, `GET` / `DELETE
//! /v1/chats/{id}/attachments/{attachment_id}` and their effect on the send
//! pipeline (DESIGN §3.3 "Upload/Get Attachment", §3.6 "File Upload", §3.7
//! `attachments` / `chat_vector_stores` / `message_attachments`, §4 "Attachment
//! Deletion", "Citation File ID and Title Resolution", ADR-0007).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use axum::http::{Method, StatusCode};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::Utc;
use mini_chat::domain::mime::XLSX_MIME;
use mini_chat::domain::services::UploadTimings;
use mini_chat::infra::db::entities::{attachment, chat, chat_vector_store, message_attachment};
use mini_chat::infra::outbox::QueueKind;
use mini_chat::testing::catalog;
use mini_chat::testing::fake_provider::{FAKE_ITEM_ID, completed_event, delta_event};
use mini_chat::testing::images::png;
use mini_chat::testing::seed::{self, NewAttachment};
use mini_chat::testing::upload::{FormPart, form_body, form_content_type};
use mini_chat::testing::{ScriptedStream, TestApp, TestAppBuilder, TestResponse, TestUser};
use mini_chat_sdk::KillSwitches;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter, QueryOrder};
use serde_json::{Value, json};
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const U: TestUser = TestUser::A1;
const PDF: &str = "application/pdf";
const PDF_BYTES: &[u8] = b"%PDF-1.4\n% mini-chat test document\n";

// ---------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------

/// Short waits so the indexing deadline and the background rounds run in tests.
fn fast_timings() -> UploadTimings {
    UploadTimings {
        indexing_deadline: Duration::from_millis(400),
        poll_initial: Duration::from_millis(20),
        poll_max: Duration::from_millis(50),
        background_round: Duration::from_millis(150),
        background_limit: Duration::from_secs(20),
        background_poll_max: Duration::from_millis(40),
        set_ready_retry: Duration::from_millis(10),
        vector_store_poll_initial: Duration::from_millis(10),
        stale_placeholder_after: Duration::from_secs(120),
    }
}

fn builder() -> TestAppBuilder {
    TestApp::builder().upload_timings(fast_timings())
}

async fn app() -> TestApp {
    builder().build().await
}

fn no_kill_switches() -> KillSwitches {
    KillSwitches {
        disable_premium_tier: false,
        force_standard_tier: false,
        disable_web_search: false,
        disable_file_search: false,
        disable_images: false,
        disable_code_interpreter: false,
    }
}

async fn create_chat_with(app: &TestApp, user: TestUser, body: Value) -> Uuid {
    let r = app.call(user, Method::POST, CHATS, Some(body)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    uuid_of(&r.json["id"])
}

async fn create_chat(app: &TestApp, user: TestUser) -> Uuid {
    create_chat_with(app, user, json!({})).await
}

fn uuid_of(v: &Value) -> Uuid {
    Uuid::parse_str(v.as_str().expect("uuid string")).expect("uuid")
}

fn attachment_path(chat: Uuid, id: Uuid) -> String {
    format!("{CHATS}/{chat}/attachments/{id}")
}

fn stream_path(chat: Uuid) -> String {
    format!("{CHATS}/{chat}/messages:stream")
}

async fn upload_pdf(app: &TestApp, chat: Uuid, name: &str) -> TestResponse {
    app.upload(U, chat, name, PDF, PDF_BYTES).await
}

/// Upload that must succeed with `ready`; returns its id.
async fn upload_ready(app: &TestApp, chat: Uuid, name: &str, ct: &str, data: &[u8]) -> Uuid {
    let r = app.upload(U, chat, name, ct, data).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "ready", "{}", r.json);
    uuid_of(&r.json["id"])
}

async fn get_attachment(app: &TestApp, user: TestUser, chat: Uuid, id: Uuid) -> TestResponse {
    app.call(user, Method::GET, &attachment_path(chat, id), None)
        .await
}

async fn delete_attachment(app: &TestApp, user: TestUser, chat: Uuid, id: Uuid) -> TestResponse {
    app.call(user, Method::DELETE, &attachment_path(chat, id), None)
        .await
}

async fn rows(app: &TestApp, chat: Uuid) -> Vec<attachment::Model> {
    let conn = app.db.conn().unwrap();
    attachment::Entity::find()
        .filter(attachment::Column::ChatId.eq(chat))
        .order_by_asc(attachment::Column::CreatedAt)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
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

async fn vector_store_rows(app: &TestApp, chat: Uuid) -> Vec<chat_vector_store::Model> {
    let conn = app.db.conn().unwrap();
    chat_vector_store::Entity::find()
        .filter(chat_vector_store::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

async fn seed_vector_store(app: &TestApp, chat: Uuid, vs: Option<&str>, provider: &str) {
    let row = chat_vector_store::Model {
        id: Uuid::new_v4(),
        tenant_id: U.tenant_id,
        chat_id: chat,
        vector_store_id: vs.map(str::to_owned),
        provider: provider.to_owned(),
        file_count: 0,
        created_at: Utc::now(),
    };
    let conn = app.db.conn().unwrap();
    secure_insert::<chat_vector_store::Entity>(
        row.into_active_model(),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
}

/// Asserts a problem with exactly one `field_violations` entry.
fn assert_field_violation(r: &TestResponse, status: StatusCode, field: &str, reason: &str) {
    assert_eq!(r.status, status, "{}", r.json);
    let v = &r.json["context"]["field_violations"];
    assert_eq!(v.as_array().map(Vec::len), Some(1), "{}", r.json);
    assert_eq!(v[0]["field"], field, "{}", r.json);
    assert_eq!(v[0]["reason"], reason, "{}", r.json);
}

fn assert_resource_type(r: &TestResponse, resource: &str) {
    assert_eq!(
        r.json["context"]["resource_type"],
        format!("gts.cf.core.mini_chat.{resource}.v1~"),
        "{}",
        r.json
    );
}

fn retry_after(r: &TestResponse) -> &str {
    r.headers
        .get("retry-after")
        .expect("Retry-After header")
        .to_str()
        .unwrap()
}

/// No provider identifier in a client-visible body.
fn assert_no_provider_ids(text: &str) {
    for marker in ["file-", "vs_", "resp_", "assistant-"] {
        assert!(
            !text.contains(marker),
            "provider id `{marker}` leaked: {text}"
        );
    }
}

/// Polls `GET` until the attachment leaves `uploaded` (at most 10 s).
async fn wait_for_final_status(app: &TestApp, chat: Uuid, id: Uuid) -> TestResponse {
    for _ in 0..500 {
        let r = get_attachment(app, U, chat, id).await;
        if r.json["status"] != "uploaded" {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("attachment {id} is still uploaded");
}

fn files_requests(app: &TestApp) -> usize {
    app.provider
        .requests()
        .iter()
        .filter(|r| r.method == Method::POST && r.path.ends_with("/v1/files"))
        .count()
}

// ---------------------------------------------------------------------------------------------
// upload: happy paths
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn upload_document_ready_and_indexed() {
    let app = app().await;
    let chat = create_chat(&app, U).await;

    let r = upload_pdf(&app, chat, "report.pdf").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = uuid_of(&r.json["id"]);
    assert_eq!(r.json["status"], "ready");
    assert_eq!(r.json["kind"], "document");
    assert_eq!(r.json["filename"], "report.pdf");
    assert_eq!(r.json["content_type"], PDF);
    assert_eq!(r.json["size_bytes"], PDF_BYTES.len());
    for absent in [
        "img_thumbnail",
        "error_code",
        "doc_summary",
        "summary_updated_at",
    ] {
        assert!(r.json.get(absent).is_none(), "{absent}: {}", r.json);
    }
    assert!(r.json["created_at"].is_string());
    assert_no_provider_ids(&r.json.to_string());

    // Files API: multipart with purpose=assistants and the structured filename.
    let upload = app
        .provider
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/v1/files"))
        .expect("files request");
    assert_eq!(upload.method, Method::POST);
    assert_eq!(upload.path, "/api.openai.com/v1/files");
    assert!(
        upload
            .content_type
            .as_deref()
            .unwrap()
            .starts_with("multipart/form-data; boundary="),
        "{upload:?}"
    );
    assert_ne!(upload.subject_id, Uuid::nil());
    let files = app.provider.files();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].purpose, "assistants");
    assert_eq!(files[0].filename, format!("{chat}_{id}.pdf"));
    assert_eq!(files[0].content_type.as_deref(), Some(PDF));
    assert_eq!(&files[0].bytes[..], PDF_BYTES);

    // Vector store created once, file added with the attachment id attribute.
    let stores = app.provider.vector_stores();
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].name, format!("chat_{chat}"));
    assert_eq!(
        stores[0].files,
        vec![(
            files[0].id.clone(),
            json!({"attachment_id": id.to_string()})
        )]
    );
    let vs_rows = vector_store_rows(&app, chat).await;
    assert_eq!(vs_rows.len(), 1);
    assert_eq!(
        vs_rows[0].vector_store_id.as_deref(),
        Some(stores[0].id.as_str())
    );
    assert_eq!(vs_rows[0].provider, "openai");
    assert_eq!(vs_rows[0].tenant_id, U.tenant_id);
    assert_eq!(vs_rows[0].file_count, 0);

    let a = row(&app, id).await;
    assert_eq!(a.status, "ready");
    assert_eq!(a.provider_file_id.as_deref(), Some(files[0].id.as_str()));
    assert_eq!(a.storage_backend, "openai");
    assert_eq!(a.uploaded_by_user_id, U.user_id);
    assert_eq!(a.tenant_id, U.tenant_id);
    assert!(a.for_file_search && !a.for_code_interpreter);
    assert_eq!(a.secondary_status, "not_attempted");
    assert!(a.error_code.is_none() && a.cleanup_status.is_none());

    // The second document reuses the chat's vector store.
    let id2 = upload_ready(&app, chat, "second.pdf", PDF, PDF_BYTES).await;
    let stores = app.provider.vector_stores();
    assert_eq!(stores.len(), 1, "{stores:?}");
    assert_eq!(stores[0].files.len(), 2);
    assert_eq!(
        stores[0].files[1].1,
        json!({"attachment_id": id2.to_string()})
    );
    assert_eq!(vector_store_rows(&app, chat).await.len(), 1);

    // GET returns the same details.
    let g = get_attachment(&app, U, chat, id).await;
    assert_eq!(g.status, StatusCode::OK, "{}", g.json);
    assert_eq!(g.json, r.json);
}

#[tokio::test]
async fn upload_image_ready_with_webp_thumbnail() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let r = app
        .upload(U, chat, "photo.png", "image/png", &png(400, 200))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "ready");
    assert_eq!(r.json["kind"], "image");
    assert_eq!(r.json["content_type"], "image/png");
    let t = &r.json["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp", "{}", r.json);
    assert_eq!(t["width"], 128);
    assert_eq!(t["height"], 64);
    let webp = STANDARD.decode(t["data_base64"].as_str().unwrap()).unwrap();
    assert_eq!(&webp[0..4], b"RIFF");
    assert_eq!(&webp[8..12], b"WEBP");
    assert!(r.json.get("error_code").is_none());

    // Uploaded to the provider, not added to any vector store.
    let files = app.provider.files();
    assert_eq!(files.len(), 1);
    let id = uuid_of(&r.json["id"]);
    assert_eq!(files[0].filename, format!("{chat}_{id}.png"));
    assert!(app.provider.vector_stores().is_empty());
    assert!(vector_store_rows(&app, chat).await.is_empty());

    let a = row(&app, id).await;
    assert!(!a.for_file_search && !a.for_code_interpreter);
    assert_eq!(a.img_thumbnail.as_deref(), Some(webp.as_slice()));
    assert_eq!(
        (a.img_thumbnail_width, a.img_thumbnail_height),
        (Some(128), Some(64))
    );

    let g = get_attachment(&app, U, chat, id).await;
    assert_eq!(g.json["img_thumbnail"], *t);
}

#[tokio::test]
async fn image_without_thumbnail_still_ready() {
    // A corrupt image cannot be thumbnailed: the attachment is still ready.
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let r = app
        .upload(U, chat, "broken.png", "image/png", b"not a png")
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "ready");
    assert!(r.json.get("img_thumbnail").is_none(), "{}", r.json);
    assert!(r.json.get("error_code").is_none(), "{}", r.json);
}

#[tokio::test]
async fn upload_xlsx_ready_not_indexed() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let id = upload_ready(&app, chat, "sheet.xlsx", XLSX_MIME, b"PK\x03\x04xlsx").await;
    let a = row(&app, id).await;
    assert_eq!(a.attachment_kind, "document");
    assert!(a.for_code_interpreter && !a.for_file_search);
    assert!(a.provider_file_id.is_some());
    assert_eq!(
        app.provider.files()[0].filename,
        format!("{chat}_{id}.xlsx")
    );
    assert!(app.provider.vector_stores().is_empty());
    assert!(vector_store_rows(&app, chat).await.is_empty());
}

#[tokio::test]
async fn xlsx_rejected_when_code_interpreter_disabled_or_unsupported() {
    // Kill switch.
    let app = builder()
        .kill_switches(KillSwitches {
            disable_code_interpreter: true,
            ..no_kill_switches()
        })
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    let r = app.upload(U, chat, "sheet.xlsx", XLSX_MIME, b"PK").await;
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "file",
        "CODE_INTERPRETER_UNAVAILABLE",
    );
    assert_resource_type(&r, "attachment");
    assert!(rows(&app, chat).await.is_empty());
    assert_eq!(files_requests(&app), 0);
    // Documents are unaffected.
    upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;

    // The chat's model has no code interpreter support.
    let app = builder().build().await;
    let chat = create_chat_with(&app, U, json!({"model": "gpt-standard"})).await;
    let r = app.upload(U, chat, "sheet.xlsx", XLSX_MIME, b"PK").await;
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "file",
        "CODE_INTERPRETER_UNAVAILABLE",
    );
    assert!(rows(&app, chat).await.is_empty());
}

#[tokio::test]
async fn image_upload_rejected_when_disable_images() {
    let app = builder()
        .kill_switches(KillSwitches {
            disable_images: true,
            ..no_kill_switches()
        })
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    let r = app
        .upload(U, chat, "photo.png", "image/png", &png(10, 10))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    let v = &r.json["context"]["violations"][0];
    assert_eq!(v["subject"], "images", "{}", r.json);
    assert_eq!(v["type"], "FEATURE_DISABLED", "{}", r.json);
    assert!(rows(&app, chat).await.is_empty());
    assert_eq!(files_requests(&app), 0);
}

// ---------------------------------------------------------------------------------------------
// upload: validation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn multipart_errors() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let file = FormPart::file(Some("a.pdf"), Some(PDF), PDF_BYTES);

    // No boundary in the Content-Type (or no Content-Type at all).
    for ct in [Some("multipart/form-data"), None, Some("application/json")] {
        let r = app
            .upload_raw(U, chat, ct, form_body(&[file.clone()]))
            .await;
        assert_field_violation(
            &r,
            StatusCode::BAD_REQUEST,
            "content_type",
            "BOUNDARY_REQUIRED",
        );
    }

    // Unreadable body.
    let r = app
        .upload_raw(
            U,
            chat,
            Some(&form_content_type()),
            b"garbage, not multipart".to_vec(),
        )
        .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "multipart", "MULTIPART_ERROR");
    let mut truncated = form_body(&[file.clone()]);
    truncated.truncate(truncated.len() - 20);
    let r = app
        .upload_raw(U, chat, Some(&form_content_type()), truncated)
        .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "multipart", "MULTIPART_ERROR");

    // No `file` part.
    let r = app
        .upload_raw(
            U,
            chat,
            Some(&form_content_type()),
            form_body(&[FormPart::text("purpose", "x")]),
        )
        .await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "file", "MISSING_FILE");

    // `file` part without a content type.
    let r = app
        .upload_raw(
            U,
            chat,
            Some(&form_content_type()),
            form_body(&[
                FormPart::text("note", "x"),
                FormPart::file(Some("a.pdf"), None, PDF_BYTES),
            ]),
        )
        .await;
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "content_type",
        "MISSING_CONTENT_TYPE",
    );
    assert_resource_type(&r, "attachment");

    assert!(rows(&app, chat).await.is_empty());
    assert!(app.provider.requests().is_empty());
}

#[tokio::test]
async fn unsupported_mime_400() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    for (name, ct) in [
        ("tool.exe", "application/x-msdownload"),
        ("clip.mp4", "video/mp4"),
        ("blob.xyz", "application/octet-stream"),
        ("noext", "application/octet-stream"),
    ] {
        let r = app.upload(U, chat, name, ct, b"data").await;
        assert_field_violation(
            &r,
            StatusCode::BAD_REQUEST,
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
        );
        assert_resource_type(&r, "attachment");
    }
    // CSV is rejected when CSV uploads are disabled.
    let strict = builder()
        .config(|c| c.rag.allow_csv_upload = false)
        .build()
        .await;
    let chat2 = create_chat(&strict, U).await;
    let r = strict
        .upload(U, chat2, "data.csv", "text/csv", b"a,b")
        .await;
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "content_type",
        "UNSUPPORTED_CONTENT_TYPE",
    );
    assert!(rows(&app, chat).await.is_empty());
    assert!(app.provider.requests().is_empty());
}

#[tokio::test]
async fn octet_stream_inferred() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    for (name, expected) in [
        ("notes.md", "text/markdown"),
        ("Report.PDF", "application/pdf"),
        ("photo.jpeg", "image/jpeg"),
    ] {
        let r = app
            .upload(U, chat, name, "application/octet-stream", &png(4, 4))
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{name}: {}", r.json);
        assert_eq!(r.json["content_type"], expected, "{name}");
    }
    // CSV is stored as text/plain; parameters are dropped.
    let r = app
        .upload(U, chat, "data.csv", "text/csv", b"a,b\n1,2")
        .await;
    assert_eq!(r.json["content_type"], "text/plain", "{}", r.json);
    let r = app
        .upload(U, chat, "readme.txt", "text/plain; charset=utf-8", b"hello")
        .await;
    assert_eq!(r.json["content_type"], "text/plain", "{}", r.json);
}

#[tokio::test]
async fn filename_defaults_strips_paths_and_truncates() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let r = app
        .upload_raw(
            U,
            chat,
            Some(&form_content_type()),
            form_body(&[FormPart::file(None, Some(PDF), PDF_BYTES)]),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["filename"], "upload");

    for (sent, stored) in [
        ("C:\\Users\\me\\report.pdf", "report.pdf"),
        ("../../etc/passwd.pdf", "passwd.pdf"),
    ] {
        let r = app.upload(U, chat, sent, PDF, PDF_BYTES).await;
        assert_eq!(r.json["filename"], stored, "{}", r.json);
    }

    let long = format!("{}.pdf", "x".repeat(300));
    let r = app.upload(U, chat, &long, PDF, PDF_BYTES).await;
    let name = r.json["filename"].as_str().unwrap();
    assert_eq!(name.chars().count(), 255);
    assert!(name.ends_with(".pdf"));
}

#[tokio::test]
async fn file_too_large_400() {
    let app = builder()
        .config(|c| {
            c.rag.uploaded_file_max_size_kb = 1;
            c.rag.uploaded_image_max_size_kb = 2;
        })
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    let r = app.upload(U, chat, "big.pdf", PDF, &[b'x'; 2048]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    assert!(
        r.json["type"].as_str().unwrap().contains("out_of_range"),
        "{}",
        r.json
    );
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "content_length",
        "FILE_TOO_LARGE",
    );
    assert_resource_type(&r, "attachment");
    assert!(rows(&app, chat).await.is_empty());
    assert_eq!(files_requests(&app), 0);

    // Exactly at the limit is accepted; images use their own limit.
    upload_ready(&app, chat, "edge.pdf", PDF, &[b'x'; 1024]).await;
    let r = app
        .upload(U, chat, "img.png", "image/png", &[0u8; 1500])
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let r = app
        .upload(U, chat, "img.png", "image/png", &[0u8; 2049])
        .await;
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "content_length",
        "FILE_TOO_LARGE",
    );

    // The model's max_file_size_mb caps the gear limits.
    let mut model = catalog::premium_model("gpt-tiny");
    model.general_config.max_file_size_mb = 0;
    let app = builder().catalog(vec![model]).build().await;
    let chat = create_chat(&app, U).await;
    let r = app.upload(U, chat, "a.pdf", PDF, b"x").await;
    assert_field_violation(
        &r,
        StatusCode::BAD_REQUEST,
        "content_length",
        "FILE_TOO_LARGE",
    );
}

#[tokio::test]
async fn document_limit_429() {
    let app = builder()
        .config(|c| c.rag.max_documents_per_chat = 1)
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    // Failed and deleted documents do not count.
    seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "failed.pdf").status("failed"),
    )
    .await;
    seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "gone.pdf").deleted(Utc::now()),
    )
    .await;
    upload_ready(&app, chat, "one.pdf", PDF, PDF_BYTES).await;
    let before = files_requests(&app);
    let r = upload_pdf(&app, chat, "two.pdf").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.json);
    assert!(
        r.json["type"]
            .as_str()
            .unwrap()
            .contains("resource_exhausted"),
        "{}",
        r.json
    );
    assert_eq!(
        r.json["context"]["violations"][0]["subject"], "document_limit",
        "{}",
        r.json
    );
    assert_eq!(files_requests(&app), before);
    assert_eq!(rows(&app, chat).await.len(), 3);
    // Images are not documents.
    upload_ready(&app, chat, "photo.png", "image/png", &png(8, 8)).await;
}

#[tokio::test]
async fn storage_limit_429() {
    let app = builder()
        .config(|c| c.rag.max_total_upload_mb_per_chat = 1)
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    let half = vec![b'x'; 600 * 1024];
    upload_ready(&app, chat, "one.pdf", PDF, &half).await;
    let r = app.upload(U, chat, "two.png", "image/png", &half).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.json);
    assert_eq!(
        r.json["context"]["violations"][0]["subject"], "storage_limit",
        "{}",
        r.json
    );
    assert_eq!(rows(&app, chat).await.len(), 1);
    // A smaller file still fits (images included in the total).
    upload_ready(&app, chat, "small.png", "image/png", &png(8, 8)).await;
}

#[tokio::test]
async fn upload_into_foreign_chat_404_chat_resource() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    for (user, chat_id) in [
        (TestUser::A2, chat),
        (TestUser::B1, chat),
        (U, Uuid::new_v4()),
    ] {
        let r = app.upload(user, chat_id, "a.pdf", PDF, PDF_BYTES).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
        assert_resource_type(&r, "chat");
    }
    // A deleted chat is not found either; the body is not even read.
    let r = app
        .call(U, Method::DELETE, &format!("{CHATS}/{chat}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.upload_raw(U, chat, None, b"garbage".to_vec()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    assert_resource_type(&r, "chat");
    assert!(app.provider.requests().is_empty());
}

#[tokio::test]
async fn upload_on_removed_model_400_invalid_model() {
    let mut old = catalog::standard_model("gpt-old");
    old.enabled = false;
    let mut cat = catalog::default_catalog();
    cat.push(old);
    let app = builder().catalog(cat).build().await;
    let chat = create_chat(&app, U).await;
    let set_model = |model: &'static str| {
        let db = app.db.clone();
        async move {
            let conn = db.conn().unwrap();
            chat::Entity::update_many()
                .col_expr(chat::Column::Model, Expr::value(model))
                .filter(chat::Column::Id.eq(chat))
                .secure()
                .scope_with(&AccessScope::allow_all())
                .exec(&conn)
                .await
                .unwrap();
        }
    };

    // Gone from the catalog: checked before the body is read.
    set_model("gpt-removed").await;
    let r = app.upload_raw(U, chat, None, b"garbage".to_vec()).await;
    assert_field_violation(&r, StatusCode::BAD_REQUEST, "model", "INVALID_MODEL");
    assert!(app.provider.requests().is_empty());

    // Disabled but still in the catalog: the upload works.
    set_model("gpt-old").await;
    upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
}

// ---------------------------------------------------------------------------------------------
// upload: provider failures and indexing
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_upload_failure_503_retry_10_row_failed() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    for status in [500, 400] {
        app.provider.fail_next("/v1/files", status);
        let r = upload_pdf(&app, chat, "report.pdf").await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.json);
        assert_eq!(retry_after(&r), "10");
        assert_eq!(r.json["detail"], "Service temporarily unavailable");
        assert_no_provider_ids(&r.json.to_string());
        assert!(!r.json.to_string().contains("injected"), "{}", r.json);
    }
    let all = rows(&app, chat).await;
    assert_eq!(all.len(), 2);
    for a in &all {
        assert_eq!(a.status, "failed");
        assert_eq!(a.error_code.as_deref(), Some("upload_failed"));
        assert!(a.provider_file_id.is_none());
        let g = get_attachment(&app, U, chat, a.id).await;
        assert_eq!(g.status, StatusCode::OK, "{}", g.json);
        assert_eq!(g.json["status"], "failed");
        assert_eq!(g.json["error_code"], "upload_failed");
    }
    assert!(app.provider.vector_stores().is_empty());
    // Failed rows do not count against the document limit; the next upload works.
    upload_ready(&app, chat, "report.pdf", PDF, PDF_BYTES).await;
}

#[tokio::test]
async fn indexing_failed_503_and_provider_file_deleted() {
    let app = app().await;
    let chat = create_chat(&app, U).await;

    // `failed` returned by the add, then `cancelled` and an unknown value while polling.
    for statuses in [
        vec!["failed"],
        vec!["in_progress", "cancelled"],
        vec!["", "weird"],
    ] {
        app.provider
            .set_vector_store_file_statuses(statuses.clone());
        let r = upload_pdf(&app, chat, "report.pdf").await;
        assert_eq!(
            r.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{statuses:?}: {}",
            r.json
        );
        assert_eq!(retry_after(&r), "10");
        assert_eq!(r.json["detail"], "Service temporarily unavailable");
        assert!(
            !r.json.to_string().contains("indexing_failed"),
            "{}",
            r.json
        );
    }
    // A failed add to the vector store.
    let vs = app.provider.vector_stores()[0].id.clone();
    app.provider
        .fail_next(&format!("/v1/vector_stores/{vs}/files"), 400);
    let r = upload_pdf(&app, chat, "report.pdf").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.json);

    let all = rows(&app, chat).await;
    assert_eq!(all.len(), 4);
    for a in &all {
        assert_eq!(a.status, "failed");
        assert_eq!(a.error_code.as_deref(), Some("indexing_failed"));
        assert!(a.cleanup_status.is_none());
        let g = get_attachment(&app, U, chat, a.id).await;
        assert_eq!(g.json["error_code"], "indexing_failed");
    }
    // Every provider file is deleted (best effort, in the background).
    for _ in 0..200 {
        if app.provider.files().iter().all(|f| f.deleted) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let files = app.provider.files();
    assert_eq!(files.len(), 4);
    assert!(files.iter().all(|f| f.deleted), "{files:?}");
}

#[tokio::test]
async fn transient_status_errors_keep_polling() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    upload_ready(&app, chat, "first.pdf", PDF, PDF_BYTES).await;
    let vs = app.provider.vector_stores()[0].id.clone();
    let status_reads = |app: &TestApp| {
        app.provider
            .requests()
            .iter()
            .filter(|r| r.method == Method::GET && r.path.contains("/vector_stores/"))
            .count()
    };

    // The add answers in_progress; the first status read fails with a provider
    // 5xx (transient): polling continues and the next read completes.
    app.provider
        .set_vector_store_file_statuses(vec!["in_progress"]);
    app.provider
        .fail_next(&format!("/v1/vector_stores/{vs}/files/"), 502);
    let before = status_reads(&app);
    let r = upload_pdf(&app, chat, "report.pdf").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "ready");
    assert_eq!(status_reads(&app) - before, 2);

    // A non-transient read error (4xx) fails the upload.
    app.provider
        .set_vector_store_file_statuses(vec!["in_progress"]);
    app.provider
        .fail_next(&format!("/v1/vector_stores/{vs}/files/"), 400);
    let r = upload_pdf(&app, chat, "bad.pdf").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.json);
    let failed = rows(&app, chat).await.into_iter().last().unwrap();
    assert_eq!(failed.error_code.as_deref(), Some("indexing_failed"));
}

#[tokio::test]
async fn indexing_still_in_progress_returns_uploaded_then_background_ready() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .set_default_vector_store_file_status("in_progress");
    let started = std::time::Instant::now();
    let r = upload_pdf(&app, chat, "slow.pdf").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["status"], "uploaded");
    assert!(started.elapsed() >= fast_timings().indexing_deadline);
    assert!(r.json.get("error_code").is_none());
    let id = uuid_of(&r.json["id"]);

    // The background task refreshes `updated_at` while it polls.
    let first = row(&app, id).await.updated_at;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let a = row(&app, id).await;
    assert_eq!(a.status, "uploaded");
    assert!(a.updated_at > first, "{} <= {first}", a.updated_at);

    // Not ready yet: a message cannot reference it.
    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": "x", "attachment_ids": [id]}),
        )
        .await;
    assert_eq!(c.status, StatusCode::BAD_REQUEST, "{c:?}");

    app.provider
        .set_default_vector_store_file_status("completed");
    let g = wait_for_final_status(&app, chat, id).await;
    assert_eq!(g.json["status"], "ready", "{}", g.json);
    assert!(app.provider.files().iter().all(|f| !f.deleted));
}

#[tokio::test]
async fn background_indexing_failure_marks_failed_and_enqueues_cleanup() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .set_default_vector_store_file_status("in_progress");
    let r = upload_pdf(&app, chat, "slow.pdf").await;
    assert_eq!(r.json["status"], "uploaded", "{}", r.json);
    let id = uuid_of(&r.json["id"]);

    app.provider.set_default_vector_store_file_status("failed");
    let g = wait_for_final_status(&app, chat, id).await;
    assert_eq!(g.json["status"], "failed", "{}", g.json);
    assert_eq!(g.json["error_code"], "indexing_failed");
    let a = row(&app, id).await;
    // Handed to cleanup; the outbox handler may already have finished it.
    assert!(
        matches!(a.cleanup_status.as_deref(), Some("pending" | "done")),
        "{a:?}"
    );
    assert!(a.deleted_at.is_none());

    let msgs = app.outbox_messages(QueueKind::AttachmentCleanup).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    let m = &msgs[0];
    assert_eq!(m["event_type"], "attachment_indexing_failed");
    assert_eq!(m["tenant_id"], U.tenant_id.to_string());
    assert_eq!(m["chat_id"], chat.to_string());
    assert_eq!(m["attachment_id"], id.to_string());
    assert_eq!(m["provider_file_id"], json!(a.provider_file_id));
    assert_eq!(m["vector_store_id"], Value::Null);
    assert_eq!(m["storage_backend"], "openai");
    assert_eq!(m["attachment_kind"], "document");
    assert_eq!(m["secondary_ref"], Value::Null);
    assert!(m["deleted_at"].is_string());
    // No inline delete: the outbox cleanup deletes the provider file.
    wait_for_files_deleted(&app).await;
    wait_for_cleanup_done(&app, id).await;
}

#[tokio::test]
async fn background_indexing_times_out() {
    let app = TestApp::builder()
        .upload_timings(UploadTimings {
            background_limit: Duration::from_millis(500),
            ..fast_timings()
        })
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    app.provider
        .set_default_vector_store_file_status("in_progress");
    let r = upload_pdf(&app, chat, "slow.pdf").await;
    let id = uuid_of(&r.json["id"]);
    let g = wait_for_final_status(&app, chat, id).await;
    assert_eq!(g.json["status"], "failed", "{}", g.json);
    assert_eq!(g.json["error_code"], "indexing_failed");
    let msgs = app.outbox_messages(QueueKind::AttachmentCleanup).await;
    assert_eq!(msgs[0]["event_type"], "attachment_indexing_failed");
}

#[tokio::test]
async fn background_indexing_stops_for_deleted_attachment() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .set_default_vector_store_file_status("in_progress");
    let r = upload_pdf(&app, chat, "a.pdf").await;
    assert_eq!(r.json["status"], "uploaded", "{}", r.json);
    let id = uuid_of(&r.json["id"]);
    let r = delete_attachment(&app, U, chat, id).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);

    // The task is still running (no gear stop): it may read `completed`, but
    // the deleted row is never made ready; the task ends.
    app.provider
        .set_default_vector_store_file_status("completed");
    wait_for_status_reads_to_stop(&app).await;
    // The cleanup handler finished the deleted row.
    wait_for_cleanup_done(&app, id).await;
    let a = row(&app, id).await;
    assert_eq!(a.status, "uploaded");
    assert!(a.deleted_at.is_some());
}

fn status_reads(app: &TestApp) -> usize {
    app.provider
        .requests()
        .iter()
        .filter(|r| {
            r.method == Method::GET
                && r.path.contains("/vector_stores/")
                && r.path.contains("/files/")
        })
        .count()
}

/// Wait (at most 10 s) until the background indexing task stopped: no provider
/// status read for two rounds.
async fn wait_for_status_reads_to_stop(app: &TestApp) {
    let quiet = fast_timings().background_round * 2;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let (mut last, mut since) = (status_reads(app), tokio::time::Instant::now());
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let n = status_reads(app);
        if n != last {
            (last, since) = (n, tokio::time::Instant::now());
        } else if since.elapsed() >= quiet {
            return;
        }
    }
    panic!("background indexing still reads the status after 10 s");
}

#[tokio::test]
async fn background_indexing_stops_on_gear_stop() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider
        .set_default_vector_store_file_status("in_progress");
    let r = upload_pdf(&app, chat, "b.pdf").await;
    assert_eq!(r.json["status"], "uploaded", "{}", r.json);
    let id = uuid_of(&r.json["id"]);
    app.stop.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let reads = app.provider.requests().len();

    app.provider
        .set_default_vector_store_file_status("completed");
    tokio::time::sleep(fast_timings().background_round * 4).await;
    // The row stays `uploaded` for the upload reaper; no more status reads.
    let a = row(&app, id).await;
    assert_eq!(a.status, "uploaded");
    assert!(a.deleted_at.is_none() && a.cleanup_status.is_none());
    assert_eq!(app.provider.requests().len(), reads);
}

/// Upload held at the provider while `during` runs; returns the upload response.
async fn upload_while<F: std::future::Future<Output = ()>>(
    app: &TestApp,
    chat: Uuid,
    during: impl FnOnce(Uuid) -> F,
) -> TestResponse {
    app.provider.hold_next("/v1/files");
    let upload = upload_pdf(app, chat, "racing.pdf");
    let other = async {
        for _ in 0..1000 {
            if files_requests(app) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let id = rows(app, chat).await.last().expect("pending row").id;
        during(id).await;
        app.provider.release_held();
    };
    tokio::join!(upload, other).0
}

async fn wait_for_files_deleted(app: &TestApp) {
    for _ in 0..200 {
        if app.provider.files().iter().all(|f| f.deleted) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("provider files not deleted: {:?}", app.provider.files());
}

/// Wait until the cleanup handler finished the attachment.
async fn wait_for_cleanup_done(app: &TestApp, id: Uuid) {
    for _ in 0..200 {
        if row(app, id).await.cleanup_status.as_deref() == Some("done") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cleanup not done: {:?}", row(app, id).await);
}

#[tokio::test]
async fn chat_deleted_during_upload_404_and_provider_file_dropped() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let r = upload_while(&app, chat, |_| async {
        let r = app
            .call(U, Method::DELETE, &format!("{CHATS}/{chat}"), None)
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    })
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    assert_resource_type(&r, "chat");
    let a = &rows(&app, chat).await[0];
    assert_eq!(a.status, "pending");
    assert!(a.provider_file_id.is_none());
    assert_eq!(a.cleanup_status.as_deref(), Some("pending"));
    // No vector store work for a deleted chat; the file is dropped.
    assert!(app.provider.vector_stores().is_empty());
    wait_for_files_deleted(&app).await;
}

#[tokio::test]
async fn attachment_deleted_during_upload_404_and_provider_file_dropped() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let r = upload_while(&app, chat, |id| {
        let app = &app;
        async move {
            let r = delete_attachment(app, U, chat, id).await;
            assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
        }
    })
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    assert_resource_type(&r, "attachment");
    let a = &rows(&app, chat).await[0];
    assert_eq!(a.status, "pending");
    assert!(a.deleted_at.is_some() && a.provider_file_id.is_none());
    assert!(app.provider.vector_stores().is_empty());
    wait_for_files_deleted(&app).await;
}

// ---------------------------------------------------------------------------------------------
// vector store protocol
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_mismatch_409() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    seed_vector_store(&app, chat, Some("vs_otherbackend0001"), "other").await;
    let r = upload_pdf(&app, chat, "a.pdf").await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.json);
    assert!(
        r.json["type"].as_str().unwrap().contains("already_exists"),
        "{}",
        r.json
    );
    assert_eq!(
        r.json["context"]["resource_name"], "provider_mismatch",
        "{}",
        r.json
    );
    assert_no_provider_ids(&r.json.to_string());
    let a = &rows(&app, chat).await[0];
    assert_eq!(a.status, "failed");
    assert_eq!(a.error_code.as_deref(), Some("vector_store_failed"));
    assert!(app.provider.vector_stores().is_empty());
    // Images do not need the vector store.
    upload_ready(&app, chat, "p.png", "image/png", &png(8, 8)).await;
}

#[tokio::test]
async fn vector_store_creation_failure_503_and_retry_creates_it() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    app.provider.fail_next("/v1/vector_stores", 500);
    let r = upload_pdf(&app, chat, "a.pdf").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.json);
    assert_eq!(retry_after(&r), "10");
    assert_eq!(
        rows(&app, chat).await[0].error_code.as_deref(),
        Some("vector_store_failed")
    );
    // The placeholder was removed: the next upload creates the store.
    assert!(vector_store_rows(&app, chat).await.is_empty());
    upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(app.provider.vector_stores().len(), 1);
    assert!(
        vector_store_rows(&app, chat).await[0]
            .vector_store_id
            .is_some()
    );
}

#[tokio::test]
async fn concurrent_first_documents_create_one_vector_store() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let (a, b, c) = tokio::join!(
        upload_pdf(&app, chat, "a.pdf"),
        upload_pdf(&app, chat, "b.pdf"),
        upload_pdf(&app, chat, "c.pdf"),
    );
    for r in [a, b, c] {
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
        assert_eq!(r.json["status"], "ready");
    }
    assert_eq!(app.provider.vector_stores().len(), 1);
    assert_eq!(app.provider.vector_stores()[0].files.len(), 3);
}

#[tokio::test]
async fn upload_concurrency_limit_503_retry_5() {
    let app = builder()
        .config(|c| c.rag.max_concurrent_uploads = 1)
        .build()
        .await;
    let chat = create_chat(&app, U).await;
    app.provider.hold_next("/v1/files");
    let first = upload_pdf(&app, chat, "first.pdf");
    let second = async {
        for _ in 0..1000 {
            if files_requests(&app) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let r = upload_pdf(&app, chat, "second.pdf").await;
        app.provider.release_held();
        r
    };
    let (first, second) = tokio::join!(first, second);
    assert_eq!(
        second.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        second.json
    );
    assert_eq!(retry_after(&second), "5");
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.json);
    assert_eq!(rows(&app, chat).await.len(), 1);
    // The permit is released with the first upload.
    upload_ready(&app, chat, "third.pdf", PDF, PDF_BYTES).await;
}

// ---------------------------------------------------------------------------------------------
// get / delete
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn get_attachment_other_uploader_404() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let other_chat = create_chat(&app, U).await;
    let foreign = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, TestUser::A2.user_id, "theirs.pdf"),
    )
    .await;
    let mine = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "mine.pdf"),
    )
    .await;

    for (chat_id, id) in [(chat, foreign), (other_chat, mine), (chat, Uuid::new_v4())] {
        for r in [
            get_attachment(&app, U, chat_id, id).await,
            delete_attachment(&app, U, chat_id, id).await,
        ] {
            assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
            assert_resource_type(&r, "attachment");
        }
    }
    // Another user's chat: 404 chat.
    let r = get_attachment(&app, TestUser::A2, chat, mine).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_resource_type(&r, "chat");

    let r = get_attachment(&app, U, chat, mine).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["filename"], "mine.pdf");
    assert!(row(&app, foreign).await.deleted_at.is_none());
}

#[tokio::test]
async fn get_deleted_404() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let gone = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, U.user_id, "gone.pdf").deleted(Utc::now()),
    )
    .await;
    let r = get_attachment(&app, U, chat, gone).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    assert_resource_type(&r, "attachment");

    let id = upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(
        delete_attachment(&app, U, chat, id).await.status,
        StatusCode::NO_CONTENT
    );
    let r = get_attachment(&app, U, chat, id).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
}

#[tokio::test]
async fn delete_unreferenced_204_enqueues_cleanup_and_repeat_204_without_new_event() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let id = upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    let r = delete_attachment(&app, U, chat, id).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);

    let a = row(&app, id).await;
    assert!(a.deleted_at.is_some());
    // Handed to cleanup; the outbox handler may already have finished it.
    assert!(
        matches!(a.cleanup_status.as_deref(), Some("pending" | "done")),
        "{a:?}"
    );
    assert!(a.cleanup_updated_at.is_some());
    assert_eq!(a.status, "ready");

    let msgs = app.outbox_messages(QueueKind::AttachmentCleanup).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    let m = &msgs[0];
    assert_eq!(m["event_type"], "attachment_deleted");
    assert_eq!(m["tenant_id"], U.tenant_id.to_string());
    assert_eq!(m["chat_id"], chat.to_string());
    assert_eq!(m["attachment_id"], id.to_string());
    assert_eq!(m["provider_file_id"], json!(a.provider_file_id));
    assert_eq!(m["vector_store_id"], Value::Null);
    assert_eq!(m["storage_backend"], "openai");
    assert_eq!(m["attachment_kind"], "document");
    assert_eq!(m["secondary_ref"], Value::Null);
    assert!(m["deleted_at"].is_string());

    // Idempotent: 204 again, no new cleanup message.
    let r = delete_attachment(&app, U, chat, id).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        app.outbox_messages(QueueKind::AttachmentCleanup)
            .await
            .len(),
        1
    );
    // The provider file is deleted by the cleanup handler, once.
    wait_for_files_deleted(&app).await;
    wait_for_cleanup_done(&app, id).await;
    let deletes = app
        .provider
        .requests()
        .iter()
        .filter(|r| r.method == Method::DELETE)
        .count();
    assert_eq!(deletes, 1);
}

#[tokio::test]
async fn delete_referenced_409_attachment_locked() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let id = upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": "see file", "attachment_ids": [id]}),
        )
        .await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    let r = delete_attachment(&app, U, chat, id).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.json);
    assert!(
        r.json["type"].as_str().unwrap().contains("already_exists"),
        "{}",
        r.json
    );
    assert_eq!(r.json["context"]["resource_name"], "attachment_locked");
    assert!(row(&app, id).await.deleted_at.is_none());
    app.assert_no_outbox(QueueKind::AttachmentCleanup, Duration::from_millis(300))
        .await;
}

// ---------------------------------------------------------------------------------------------
// send pipeline with uploaded attachments
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn file_search_tool_sent_only_with_ready_docs() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let send = |content: &'static str| {
        let path = stream_path(chat);
        let app = &app;
        async move { app.stream(U, &path, json!({"content": content})).await }
    };

    assert_eq!(send("before").await.status, StatusCode::OK);
    assert!(app.provider.chat_requests()[0].get("tools").is_none());

    let id = upload_ready(&app, chat, "a.pdf", PDF, PDF_BYTES).await;
    let vs = app.provider.vector_stores()[0].id.clone();
    assert_eq!(send("with doc").await.status, StatusCode::OK);
    let req = &app.provider.chat_requests()[1];
    assert_eq!(
        req["tools"],
        json!([{"type": "file_search", "vector_store_ids": [vs], "max_num_results": 5}])
    );
    assert_eq!(req["metadata"]["feature"], "file_search");
    assert!(
        req["instructions"]
            .as_str()
            .unwrap()
            .contains(&app.config.context.file_search_guard),
        "{req}"
    );

    assert_eq!(
        delete_attachment(&app, U, chat, id).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(send("after delete").await.status, StatusCode::OK);
    assert!(app.provider.chat_requests()[2].get("tools").is_none());
}

#[tokio::test]
async fn code_interpreter_tool_with_ready_xlsx() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    upload_ready(&app, chat, "sheet.xlsx", XLSX_MIME, b"PK").await;
    let file_id = app.provider.files()[0].id.clone();
    let c = app
        .stream(U, &stream_path(chat), json!({"content": "sum it"}))
        .await;
    assert_eq!(c.status, StatusCode::OK);
    let req = &app.provider.chat_requests()[0];
    assert_eq!(
        req["tools"],
        json!([{"type": "code_interpreter", "container": {"type": "auto", "file_ids": [file_id]}}])
    );
    assert_eq!(req["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(req["metadata"]["feature"], "code_interpreter");
}

#[tokio::test]
async fn image_attached_to_message_sent_as_input_image_once() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let img = upload_ready(&app, chat, "p.png", "image/png", &png(20, 20)).await;
    let file_id = app.provider.files()[0].id.clone();

    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": "what is this?", "attachment_ids": [img]}),
        )
        .await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");
    let req = &app.provider.chat_requests()[0];
    let input = req["input"].as_array().unwrap();
    assert_eq!(
        input.last().unwrap()["content"],
        json!([
            {"type": "input_text", "text": "what is this?"},
            {"type": "input_image", "file_id": file_id},
        ])
    );

    let c = app
        .stream(U, &stream_path(chat), json!({"content": "and now?"}))
        .await;
    assert_eq!(c.status, StatusCode::OK);
    let req = app.provider.chat_requests()[1].to_string();
    assert!(!req.contains("input_image"), "{req}");
    assert!(app.provider.vector_stores().is_empty());
    assert!(app.provider.chat_requests()[1].get("tools").is_none());
}

#[tokio::test]
async fn file_citation_mapped_to_attachment_id_and_filename() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let id = upload_ready(&app, chat, "handbook.pdf", PDF, PDF_BYTES).await;
    let file_id = app.provider.files()[0].id.clone();
    let annotation = |fid: &str| {
        (
            "response.output_text.annotation.added".to_owned(),
            json!({
                "type": "response.output_text.annotation.added",
                "item_id": FAKE_ITEM_ID, "output_index": 0, "content_index": 0,
                "annotation": {"type": "file_citation", "file_id": fid, "filename": "x.pdf", "index": 2},
            }),
        )
    };
    app.provider.push_stream(ScriptedStream::events(vec![
        delta_event("Per the handbook."),
        annotation(&file_id),
        annotation("file-unknown0000000000"),
        completed_event("Per the handbook.", 10, 5),
    ]));
    let c = app
        .stream(U, &stream_path(chat), json!({"content": "q"}))
        .await;
    assert_eq!(c.status, StatusCode::OK);
    assert_eq!(
        c.first("citations").cloned(),
        Some(json!({"items": [{
            "source": "file", "title": "handbook.pdf",
            "attachment_id": id.to_string(), "snippet": "",
        }]})),
        "{c:?}"
    );
    let wire = serde_json::to_string(&c.events).unwrap();
    assert_no_provider_ids(&wire);
}

#[tokio::test]
async fn message_attachments_recorded_and_listed() {
    let app = app().await;
    let chat = create_chat(&app, U).await;
    let doc = upload_ready(&app, chat, "doc.pdf", PDF, PDF_BYTES).await;
    let img = upload_ready(&app, chat, "pic.png", "image/png", &png(300, 150)).await;
    let c = app
        .stream(
            U,
            &stream_path(chat),
            json!({"content": "both", "attachment_ids": [doc, img]}),
        )
        .await;
    assert_eq!(c.status, StatusCode::OK, "{c:?}");

    let conn = app.db.conn().unwrap();
    let links = message_attachment::Entity::find()
        .filter(message_attachment::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    assert_eq!(links.len(), 2);
    assert!(links.iter().all(|l| l.tenant_id == U.tenant_id));

    let r = app
        .call(U, Method::GET, &format!("{CHATS}/{chat}/messages"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let items = r.json["items"].as_array().unwrap();
    let user_msg = items.iter().find(|m| m["role"] == "user").unwrap();
    // Links of one message share `created_at`, so their order is not fixed.
    let mut atts = user_msg["attachments"].as_array().unwrap().clone();
    atts.sort_by_key(|a| a["kind"].as_str().unwrap().to_owned());
    assert_eq!(atts.len(), 2, "{}", r.json);
    assert_eq!(atts[0]["attachment_id"], doc.to_string());
    assert_eq!(atts[0]["kind"], "document");
    assert_eq!(atts[0]["filename"], "doc.pdf");
    assert_eq!(atts[0]["status"], "ready");
    assert!(atts[0].get("img_thumbnail").is_none());
    assert_eq!(atts[1]["attachment_id"], img.to_string());
    assert_eq!(atts[1]["kind"], "image");
    assert_eq!(atts[1]["img_thumbnail"]["width"], 128);
    assert_eq!(atts[1]["img_thumbnail"]["height"], 64);
    let assistant = items.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["attachments"], json!([]));
    assert_no_provider_ids(&r.json.to_string());
}

#[tokio::test]
async fn openapi_declares_attachment_operations() {
    use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};

    let app = app().await;
    let registry = OpenApiRegistryImpl::new();
    let _router = mini_chat::api::rest::routes::register_routes(
        axum::Router::new(),
        &registry,
        app.services.clone(),
        &app.config,
    );
    let doc =
        serde_json::to_value(registry.build_openapi(&OpenApiInfo::default()).unwrap()).unwrap();
    let paths = &doc["paths"];
    let upload = &paths["/mini-chat/v1/chats/{id}/attachments"]["post"];
    assert_eq!(
        upload["operationId"], "mini_chat.upload_attachment",
        "{upload}"
    );
    assert!(
        upload["requestBody"]["content"]["multipart/form-data"].is_object(),
        "{upload}"
    );
    assert!(upload["responses"]["201"].is_object(), "{upload}");
    let one = &paths["/mini-chat/v1/chats/{id}/attachments/{attachment_id}"];
    assert_eq!(one["get"]["operationId"], "mini_chat.get_attachment");
    assert_eq!(one["delete"]["operationId"], "mini_chat.delete_attachment");
    let detail = &doc["components"]["schemas"]["AttachmentDetailDto"];
    assert_eq!(
        detail["required"],
        json!([
            "id",
            "filename",
            "content_type",
            "size_bytes",
            "status",
            "kind",
            "created_at"
        ]),
        "{detail}"
    );
}
