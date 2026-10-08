//! End-to-end tests of the attachment endpoints (upload, get, delete) and of the way ready
//! attachments reach the provider request.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::Method;
use mini_chat_sdk::KillSwitches;
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{DEFAULT_FILE_SEARCH_GUARD, DEFAULT_WEB_SEARCH_GUARD};
use crate::domain::attachment::IndexingTimings;
use crate::infra::db::ts::db_now;
use crate::test_support::app::{NO_KILL_SWITCHES, TestApp, TestResponse, ctx, test_config};
use crate::test_support::attachments::{
    BOUNDARY, CLEANUP_QUEUE, FILES_PATH, PDF, Part, VS_ID, XLSX, attachment_row, attachment_rows,
    attachment_uri, delete_vector_store_rows, fast_timings, id_of, multipart_body, png,
    post_upload, script_file_deletes, script_files, script_index_status, script_vector_store,
    seed_vector_store_row, seed_vector_store_row_at, upload, vector_store_creates,
    vector_store_rows,
};
use crate::test_support::catalog::{no_vision_model, standard_model};
use crate::test_support::gateway::Responder;
use crate::test_support::stream::{
    CHATS, SeedAttachment, answer, create_chat, provider_calls, script_provider, seed_attachment,
    soft_delete_attachment, stream_uri,
};

const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";
const ATTACHMENT_RESOURCE: &str = "gts.cf.core.mini_chat.attachment.v1~";
const MULTIPART: &str = "multipart/form-data; boundary=mini-chat-test-boundary";

fn user() -> SecurityContext {
    ctx(Uuid::new_v4(), Uuid::new_v4())
}

fn completed() -> Value {
    json!({"id": "file-x", "object": "vector_store.file", "status": "completed"})
}

fn in_progress() -> Value {
    json!({"id": "file-x", "object": "vector_store.file", "status": "in_progress"})
}

/// Storage answers for a document that indexes at once into [`VS_ID`].
fn script_ready_documents(app: &TestApp, file_ids: &[&str]) {
    script_files(app, file_ids);
    script_vector_store(app, VS_ID, Duration::ZERO, completed());
}

fn field_violation(res: &TestResponse) -> (&str, &str) {
    let v = &res.json["context"]["field_violations"][0];
    (
        v["field"].as_str().unwrap_or_default(),
        v["reason"].as_str().unwrap_or_default(),
    )
}

fn assert_category(res: &TestResponse, status: u16, category: &str) {
    assert_eq!(res.status, status, "{}", res.json);
    let ty = res.json["type"].as_str().unwrap_or_default();
    assert!(
        ty.ends_with(&format!("cf.core.err.{category}.v1~")),
        "{category}: {}",
        res.json
    );
}

async fn get(app: &TestApp, who: &SecurityContext, chat: Uuid, id: Uuid) -> TestResponse {
    app.call("GET", &attachment_uri(chat, id), who, None).await
}

async fn delete(app: &TestApp, who: &SecurityContext, chat: Uuid, id: Uuid) -> TestResponse {
    app.call("DELETE", &attachment_uri(chat, id), who, None)
        .await
}

fn config(edit: impl FnOnce(&mut crate::config::MiniChatConfig)) -> crate::config::MiniChatConfig {
    let mut cfg = test_config();
    edit(&mut cfg);
    cfg
}

/// Waits until the attachment's `GET` reports `status`.
async fn wait_for_status(app: &TestApp, who: &SecurityContext, chat: Uuid, id: Uuid, status: &str) {
    TestApp::wait_until(&format!("attachment becomes {status}"), || async {
        get(app, who, chat, id).await.json["status"] == status
    })
    .await;
}

#[tokio::test]
async fn upload_document_ready() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_ready_documents(&app, &["file-doc1", "file-doc2"]);

    let res = upload(&app, &who, chat, "report.pdf", PDF, b"%PDF-1.4 one").await;

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "ready");
    assert_eq!(res.json["kind"], "document");
    assert_eq!(res.json["filename"], "report.pdf");
    assert_eq!(res.json["content_type"], PDF);
    assert_eq!(res.json["size_bytes"], 12);
    for absent in [
        "doc_summary",
        "img_thumbnail",
        "summary_updated_at",
        "error_code",
    ] {
        assert!(res.json.get(absent).is_none(), "{absent} in {}", res.json);
    }
    assert!(!res.json.to_string().contains("file-doc1"), "{}", res.json);
    let att = id_of(&res);

    let files = app.gateway.requests_to(&Method::POST, FILES_PATH);
    assert_eq!(files.len(), 1);
    let body = String::from_utf8_lossy(&files[0].body);
    assert!(body.contains("name=\"purpose\""), "{body}");
    assert!(body.contains("assistants"), "{body}");
    assert!(
        body.contains(&format!("filename=\"{chat}_{att}.pdf\"")),
        "{body}"
    );
    assert!(body.contains("%PDF-1.4 one"), "{body}");
    assert_eq!(vector_store_creates(&app), 1);
    let adds = app
        .gateway
        .requests_to(&Method::POST, &format!("/vector_stores/{VS_ID}/files"));
    assert_eq!(adds.len(), 1);
    let add = adds[0].json.as_ref().expect("JSON add_file");
    assert_eq!(add["file_id"], "file-doc1");
    assert_eq!(add["attributes"]["attachment_id"], att.to_string());

    let row = attachment_row(&app, chat, att).await;
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.provider_file_id.as_deref(), Some("file-doc1"));
    assert_eq!(row.status, "ready");
    assert_eq!(row.storage_backend, "openai");
    assert_eq!(row.uploaded_by_user_id, who.subject_id());
    assert_eq!(row.attachment_kind, "document");
    let stores = vector_store_rows(&app, chat).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some(VS_ID));
    assert_eq!(stores[0].provider, "openai");

    let fetched = get(&app, &who, chat, att).await;
    assert_eq!(fetched.status, 200, "{}", fetched.json);
    assert_eq!(fetched.json, res.json, "GET shows what the upload returned");

    let second = upload(&app, &who, chat, "notes.txt", "text/plain", b"notes").await;
    assert_eq!(second.status, 201, "{}", second.json);
    assert_eq!(second.json["status"], "ready");
    assert_eq!(vector_store_creates(&app), 1, "the chat's store is reused");
    let adds = app
        .gateway
        .requests_to(&Method::POST, &format!("/vector_stores/{VS_ID}/files"));
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[1].json.as_ref().unwrap()["file_id"], "file-doc2");
}

#[tokio::test]
async fn upload_document_indexing_in_progress_then_ready() {
    let app = TestApp::builder()
        .indexing_timings(fast_timings(Duration::from_secs(10)))
        .build()
        .await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, in_progress());
    script_index_status(&app, VS_ID, in_progress());

    let res = upload(&app, &who, chat, "report.pdf", PDF, b"%PDF").await;

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "uploaded");
    let att = id_of(&res);
    assert_eq!(attachment_row(&app, chat, att).await.status, "uploaded");
    assert!(
        !app.gateway
            .requests_to(
                &Method::GET,
                &format!("/vector_stores/{VS_ID}/files/file-doc1")
            )
            .is_empty(),
        "the request polled the status before its deadline"
    );

    script_index_status(&app, VS_ID, completed());
    wait_for_status(&app, &who, chat, att, "ready").await;
    let row = attachment_row(&app, chat, att).await;
    assert_eq!((row.error_code, row.cleanup_status), (None, None));
}

#[tokio::test]
async fn upload_indexing_failed_returns_503() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1", "file-doc2"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, in_progress());
    script_index_status(
        &app,
        VS_ID,
        json!({"status": "failed", "last_error": {"message": "file-doc1 is broken"}}),
    );
    script_file_deletes(&app);

    let res = upload(&app, &who, chat, "report.pdf", PDF, b"%PDF").await;

    assert_category(&res, 503, "service_unavailable");
    assert_eq!(res.headers["retry-after"], "10");
    let text = res.json.to_string();
    assert!(
        !text.contains("indexing_failed") && !text.contains("file-doc1"),
        "{text}"
    );
    let rows = attachment_rows(&app, chat).await;
    assert_eq!(rows.len(), 1);
    let att = rows[0].id;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("indexing_failed"));
    let fetched = get(&app, &who, chat, att).await;
    assert_eq!(fetched.status, 200, "{}", fetched.json);
    assert_eq!(fetched.json["status"], "failed");
    assert_eq!(fetched.json["error_code"], "indexing_failed");
    TestApp::wait_until("the provider file is deleted", || async {
        !app.gateway
            .requests_to(&Method::DELETE, "/v1/files/file-doc1")
            .is_empty()
    })
    .await;

    // A rejected add to the vector store fails the same way.
    app.gateway.on(
        Method::POST,
        &format!("/vector_stores/{VS_ID}/files"),
        Responder::json(400, json!({"error": {"message": "unsupported file"}})),
    );
    let res = upload(&app, &who, chat, "other.pdf", PDF, b"%PDF").await;
    assert_category(&res, 503, "service_unavailable");
    let rows = attachment_rows(&app, chat).await;
    let second = rows.iter().find(|r| r.id != att).expect("second row");
    assert_eq!(second.status, "failed");
    assert_eq!(second.error_code.as_deref(), Some("indexing_failed"));
}

#[tokio::test]
async fn background_indexing_timeout_fails_and_enqueues_cleanup() {
    let app = TestApp::builder()
        .quiet_cleanup()
        .indexing_timings(fast_timings(Duration::from_millis(300)))
        .build()
        .await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, in_progress());
    script_index_status(&app, VS_ID, in_progress());

    let res = upload(&app, &who, chat, "report.pdf", PDF, b"%PDF").await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "uploaded");
    let att = id_of(&res);

    wait_for_status(&app, &who, chat, att, "failed").await;
    let row = attachment_row(&app, chat, att).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.deleted_at.is_none(), "failed, not deleted");
    TestApp::wait_until("the cleanup event is delivered", || async {
        !app.outbox_payloads(CLEANUP_QUEUE).is_empty()
    })
    .await;
    let payloads = app.outbox_payloads(CLEANUP_QUEUE);
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];
    assert_eq!(p["event_type"], "attachment_indexing_failed");
    assert_eq!(p["attachment_id"], att.to_string());
    assert_eq!(p["chat_id"], chat.to_string());
    assert_eq!(p["tenant_id"], who.subject_tenant_id().to_string());
    assert_eq!(p["provider_file_id"], "file-doc1");
    assert_eq!(p["storage_backend"], "openai");
    assert_eq!(p["attachment_kind"], "document");
    assert!(p["vector_store_id"].is_null() && p["secondary_ref"].is_null());
}

#[tokio::test]
async fn background_indexing_stops_for_deleted_attachment_and_on_shutdown() {
    let app = TestApp::builder()
        .indexing_timings(fast_timings(Duration::from_secs(10)))
        .build()
        .await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1", "file-doc2"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, in_progress());
    script_index_status(&app, VS_ID, in_progress());
    let deleted = id_of(&upload(&app, &who, chat, "a.pdf", PDF, b"%PDF").await);
    assert_eq!(delete(&app, &who, chat, deleted).await.status, 204);
    script_index_status(&app, VS_ID, completed());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        attachment_row(&app, chat, deleted).await.status,
        "uploaded",
        "a deleted attachment never becomes ready"
    );

    script_index_status(&app, VS_ID, in_progress());
    let stopped = id_of(&upload(&app, &who, chat, "b.pdf", PDF, b"%PDF").await);
    app.services.attachments.shutdown();
    script_index_status(&app, VS_ID, completed());
    tokio::time::sleep(Duration::from_millis(400)).await;
    let row = attachment_row(&app, chat, stopped).await;
    assert_eq!(row.status, "uploaded", "the cancelled task changes nothing");
    assert!(row.cleanup_status.is_none());
}

#[tokio::test]
async fn upload_image_with_thumbnail() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-img1"]);

    let res = upload(&app, &who, chat, "photo.png", "image/png", &png(300, 150)).await;

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["kind"], "image");
    assert_eq!(res.json["status"], "ready");
    let thumb = &res.json["img_thumbnail"];
    assert_eq!(thumb["content_type"], "image/webp");
    assert_eq!(
        (thumb["width"].as_i64(), thumb["height"].as_i64()),
        (Some(128), Some(64))
    );
    let webp = STANDARD
        .decode(thumb["data_base64"].as_str().expect("data_base64"))
        .expect("base64");
    assert_eq!((&webp[..4], &webp[8..12]), (&b"RIFF"[..], &b"WEBP"[..]));
    assert!(
        app.gateway
            .requests_to(&Method::POST, "/vector_stores")
            .is_empty()
    );
    let att = id_of(&res);
    let row = attachment_row(&app, chat, att).await;
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.img_thumbnail.as_deref(), Some(&webp[..]));
    assert_eq!(
        (row.img_thumbnail_width, row.img_thumbnail_height),
        (Some(128), Some(64))
    );
    assert_eq!(get(&app, &who, chat, att).await.json, res.json);

    // An image that cannot be decoded is still ready, without a preview.
    script_files(&app, &["file-img2"]);
    let res = upload(&app, &who, chat, "broken.png", "image/png", b"not a png").await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "ready");
    assert!(res.json.get("img_thumbnail").is_none(), "{}", res.json);
}

#[tokio::test]
async fn upload_xlsx_routes_to_code_interpreter() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-xlsx1"]);

    let res = upload(&app, &who, chat, "data.xlsx", XLSX, b"PK xlsx").await;

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(
        (&res.json["status"], &res.json["kind"]),
        (&json!("ready"), &json!("document"))
    );
    let row = attachment_row(&app, chat, id_of(&res)).await;
    assert!(row.for_code_interpreter && !row.for_file_search);
    assert!(
        app.gateway
            .requests_to(&Method::POST, "/vector_stores")
            .is_empty()
    );

    app.usage.set_kill_switches(KillSwitches {
        disable_code_interpreter: true,
        ..NO_KILL_SWITCHES
    });
    let res = upload(&app, &who, chat, "data.xlsx", XLSX, b"PK xlsx").await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res).1, "CODE_INTERPRETER_UNAVAILABLE");

    app.usage.set_kill_switches(NO_KILL_SWITCHES);
    let novision = create_chat(&app, &who, Some("gpt-mini-novision")).await;
    let res = upload(&app, &who, novision, "data.xlsx", XLSX, b"PK xlsx").await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res).1, "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(
        app.gateway.requests_to(&Method::POST, FILES_PATH).len(),
        1,
        "rejected uploads never reach the provider"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one table of rejections
async fn upload_validation_errors() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    let pdf = |data: &[u8]| multipart_body(&[Part::file(Some("r.pdf"), Some(PDF), data)]);

    let res = post_upload(&app, &who, chat, "multipart/form-data", pdf(b"x")).await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res), ("content_type", "BOUNDARY_REQUIRED"));

    let res = post_upload(
        &app,
        &who,
        chat,
        MULTIPART,
        multipart_body(&[Part::text("note", "no file here")]),
    )
    .await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res), ("file", "MISSING_FILE"));

    let res = post_upload(
        &app,
        &who,
        chat,
        MULTIPART,
        multipart_body(&[Part::file(Some("r.pdf"), None, b"x")]),
    )
    .await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(
        field_violation(&res),
        ("content_type", "MISSING_CONTENT_TYPE")
    );

    let res = upload(
        &app,
        &who,
        chat,
        "setup.exe",
        "application/x-msdownload",
        b"MZ",
    )
    .await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res).1, "UNSUPPORTED_CONTENT_TYPE");

    let res = upload(
        &app,
        &who,
        chat,
        "blob.bin",
        "application/octet-stream",
        b"?",
    )
    .await;
    assert_eq!(
        field_violation(&res).1,
        "UNSUPPORTED_CONTENT_TYPE",
        "{}",
        res.json
    );

    let truncated = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"");
    let res = post_upload(&app, &who, chat, MULTIPART, truncated.into_bytes()).await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res), ("multipart", "MULTIPART_ERROR"));
    assert!(
        app.gateway.requests().is_empty(),
        "nothing reached the provider"
    );

    script_ready_documents(&app, &["file-doc1"]);
    let res = upload(
        &app,
        &who,
        chat,
        "report.pdf",
        "application/octet-stream",
        b"%PDF",
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["content_type"], PDF);

    app.usage.set_kill_switches(KillSwitches {
        disable_images: true,
        ..NO_KILL_SWITCHES
    });
    let res = upload(&app, &who, chat, "p.png", "image/png", &png(4, 4)).await;
    assert_category(&res, 400, "failed_precondition");
    let v = &res.json["context"]["violations"][0];
    assert_eq!(
        (&v["subject"], &v["type"]),
        (&json!("images"), &json!("FEATURE_DISABLED"))
    );
    app.usage.set_kill_switches(NO_KILL_SWITCHES);

    // The chat and its model are checked before the body is read: a body that would be
    // rejected (no boundary, truncated multipart) does not change the answer.
    let stranger = user();
    for res in [
        upload(&app, &stranger, chat, "r.pdf", PDF, b"%PDF").await,
        post_upload(&app, &stranger, chat, "multipart/form-data", pdf(b"x")).await,
    ] {
        assert_eq!(res.status, 404, "{}", res.json);
        assert_eq!(res.json["context"]["resource_type"], CHAT_RESOURCE);
    }

    let standard = create_chat(&app, &who, Some("gpt-standard")).await;
    app.usage.set_catalog(vec![
        standard_model("gpt-premium"),
        no_vision_model("gpt-mini-novision"),
    ]);
    app.gateway.clear_requests();
    let res = upload(&app, &who, standard, "r.pdf", PDF, b"%PDF").await;
    assert_category(&res, 400, "invalid_argument");
    assert_eq!(field_violation(&res), ("model", "INVALID_MODEL"));
    let truncated = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"");
    let res = post_upload(&app, &who, standard, MULTIPART, truncated.into_bytes()).await;
    assert_eq!(
        field_violation(&res),
        ("model", "INVALID_MODEL"),
        "{}",
        res.json
    );
    assert!(
        app.gateway.requests().is_empty(),
        "nothing reached the provider"
    );
    assert!(attachment_rows(&app, standard).await.is_empty());

    // The image limit is the smaller of the gear and the model limits.
    let small = TestApp::builder()
        .config(config(|c| c.rag.uploaded_image_max_size_kb = 1))
        .build()
        .await;
    let chat = create_chat(&small, &who, None).await;
    let res = upload(&small, &who, chat, "p.png", "image/png", &[0_u8; 1025]).await;
    assert_category(&res, 400, "out_of_range");
    assert_eq!(field_violation(&res), ("content_length", "FILE_TOO_LARGE"));
    assert!(small.gateway.requests().is_empty());
    let mut tiny_model = standard_model("gpt-premium");
    tiny_model.general_config.max_file_size_mb = 1;
    small.usage.set_catalog(vec![tiny_model]);
    let res = upload(
        &small,
        &who,
        chat,
        "big.pdf",
        PDF,
        &vec![b'x'; 1024 * 1024 + 1],
    )
    .await;
    assert_eq!(
        field_violation(&res),
        ("content_length", "FILE_TOO_LARGE"),
        "{}",
        res.json
    );
}

#[tokio::test]
async fn per_chat_limits() {
    let app = TestApp::builder()
        .config(config(|c| c.rag.max_documents_per_chat = 1))
        .build()
        .await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    app.gateway.on_sequence(
        Method::POST,
        FILES_PATH,
        vec![Responder::json(500, json!({}))],
    );
    let res = upload(&app, &who, chat, "a.pdf", PDF, b"%PDF").await;
    assert_category(&res, 503, "service_unavailable");
    let failed = attachment_rows(&app, chat).await;
    assert_eq!(failed[0].status, "failed");
    assert_eq!(failed[0].error_code.as_deref(), Some("upload_failed"));

    script_ready_documents(&app, &["file-doc1", "file-img1"]);
    let res = upload(&app, &who, chat, "b.pdf", PDF, b"%PDF").await;
    assert_eq!(res.status, 201, "a failed row does not count: {}", res.json);
    let res = upload(&app, &who, chat, "c.pdf", PDF, b"%PDF").await;
    assert_category(&res, 429, "resource_exhausted");
    assert_eq!(
        res.json["context"]["violations"][0]["subject"],
        "document_limit"
    );
    let res = upload(&app, &who, chat, "p.png", "image/png", &png(2, 2)).await;
    assert_eq!(
        res.status, 201,
        "images do not count as documents: {}",
        res.json
    );
    assert_eq!(app.gateway.requests_to(&Method::POST, FILES_PATH).len(), 3);

    let app = TestApp::builder()
        .config(config(|c| c.rag.max_total_upload_mb_per_chat = 1))
        .build()
        .await;
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-img1", "file-img2"]);
    let image = vec![7_u8; 600 * 1024];
    let res = upload(&app, &who, chat, "a.png", "image/png", &image).await;
    assert_eq!(res.status, 201, "{}", res.json);
    let res = upload(&app, &who, chat, "b.png", "image/png", &image).await;
    assert_category(&res, 429, "resource_exhausted");
    assert_eq!(
        res.json["context"]["violations"][0]["subject"],
        "storage_limit"
    );
    assert_eq!(attachment_rows(&app, chat).await.len(), 1);
}

#[tokio::test]
async fn upload_concurrency_limit() {
    let app = Arc::new(
        TestApp::builder()
            .config(config(|c| c.rag.max_concurrent_uploads = 1))
            .build()
            .await,
    );
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    app.gateway
        .on_sequence(Method::POST, FILES_PATH, vec![Responder::Hang]);
    let blocked = tokio::spawn({
        let (app, who) = (Arc::clone(&app), who.clone());
        async move { upload(&app, &who, chat, "a.png", "image/png", &png(2, 2)).await }
    });
    TestApp::wait_until("the first upload reaches the provider", || async {
        !app.gateway
            .requests_to(&Method::POST, FILES_PATH)
            .is_empty()
    })
    .await;

    let res = upload(&app, &who, chat, "b.png", "image/png", &png(2, 2)).await;
    assert_category(&res, 503, "service_unavailable");
    assert_eq!(res.headers["retry-after"], "5");

    blocked.abort();
    assert!(blocked.await.is_err(), "the first upload was aborted");
    script_files(&app, &["file-img2"]);
    let res = upload(&app, &who, chat, "c.png", "image/png", &png(2, 2)).await;
    assert_eq!(res.status, 201, "the slot is free again: {}", res.json);
}

#[tokio::test]
async fn provider_mismatch_conflict() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    seed_vector_store_row(
        &app,
        who.subject_tenant_id(),
        chat,
        Some("vs_other"),
        "other",
    )
    .await;
    script_ready_documents(&app, &["file-doc1"]);

    let res = upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await;

    assert_category(&res, 409, "already_exists");
    assert_eq!(res.json["context"]["resource_name"], "provider_mismatch");
    assert!(
        app.gateway.requests().is_empty(),
        "checked before the provider upload"
    );
}

#[tokio::test]
async fn long_multibyte_filename_truncated_keeping_extension() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_ready_documents(&app, &["file-doc1", "file-doc2"]);
    let long = format!("{}.pdf", "\u{439}".repeat(300));

    let res = upload(&app, &who, chat, &long, PDF, b"%PDF").await;

    assert_eq!(res.status, 201, "{}", res.json);
    let stored = res.json["filename"].as_str().unwrap().to_owned();
    assert_eq!(stored.chars().count(), 255);
    assert_eq!(
        stored.rsplit_once('.').map(|(_, ext)| ext),
        Some("pdf"),
        "{stored}"
    );
    assert!(stored.starts_with('\u{439}'));
    let row = attachment_row(&app, chat, id_of(&res)).await;
    assert_eq!(row.filename, stored);

    let res = post_upload(
        &app,
        &who,
        chat,
        MULTIPART,
        multipart_body(&[Part::file(None, Some(PDF), b"%PDF")]),
    )
    .await;
    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["filename"], "upload");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one lifecycle scenario
async fn get_and_delete_rules() {
    let app = TestApp::builder().quiet_cleanup().build().await;
    let who = user();
    let tenant = who.subject_tenant_id();
    let chat = create_chat(&app, &who, None).await;

    let foreign = seed_attachment(&app, SeedAttachment::image(tenant, chat, Uuid::new_v4())).await;
    for res in [
        get(&app, &who, chat, foreign).await,
        delete(&app, &who, chat, foreign).await,
    ] {
        assert_eq!(res.status, 404, "{}", res.json);
        assert_eq!(res.json["context"]["resource_type"], ATTACHMENT_RESOURCE);
    }
    // The uploader check comes before the idempotency of a repeated delete.
    let foreign_deleted =
        seed_attachment(&app, SeedAttachment::image(tenant, chat, Uuid::new_v4())).await;
    soft_delete_attachment(&app, foreign_deleted).await;
    let res = delete(&app, &who, chat, foreign_deleted).await;
    assert_eq!(res.status, 404, "{}", res.json);
    assert_eq!(res.json["context"]["resource_type"], ATTACHMENT_RESOURCE);
    let unknown = get(&app, &who, chat, Uuid::new_v4()).await;
    assert_eq!(
        unknown.json["context"]["resource_type"],
        ATTACHMENT_RESOURCE
    );
    let other_chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-img1", "file-img2"]);
    let own = id_of(&upload(&app, &who, chat, "a.png", "image/png", &png(2, 2)).await);
    assert_eq!(
        get(&app, &who, other_chat, own).await.status,
        404,
        "an attachment of another chat"
    );

    let res = delete(&app, &who, chat, own).await;
    assert_eq!(res.status, 204, "{}", res.json);
    let row = attachment_row(&app, chat, own).await;
    assert!(row.deleted_at.is_some());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    TestApp::wait_until("the cleanup event is delivered", || async {
        !app.outbox_payloads(CLEANUP_QUEUE).is_empty()
    })
    .await;
    let p = app.outbox_payloads(CLEANUP_QUEUE)[0].clone();
    assert_eq!(p["event_type"], "attachment_deleted");
    assert_eq!(p["attachment_id"], own.to_string());
    assert_eq!(p["chat_id"], chat.to_string());
    assert_eq!(p["tenant_id"], tenant.to_string());
    assert_eq!(p["provider_file_id"], "file-img1");
    assert_eq!(p["storage_backend"], "openai");
    assert_eq!(p["attachment_kind"], "image");
    assert!(p["vector_store_id"].is_null() && p["secondary_ref"].is_null());

    assert_eq!(
        delete(&app, &who, chat, own).await.status,
        204,
        "idempotent"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        app.outbox_payloads(CLEANUP_QUEUE).len(),
        1,
        "no second event"
    );
    let gone = get(&app, &who, chat, own).await;
    assert_eq!(gone.status, 404);
    assert_eq!(gone.json["context"]["resource_type"], ATTACHMENT_RESOURCE);

    let sent = id_of(&upload(&app, &who, chat, "b.png", "image/png", &png(2, 2)).await);
    script_provider(&app, answer(&["seen"], 3, 2));
    app.stream(
        "POST",
        &stream_uri(chat),
        &who,
        json!({"content": "look", "attachment_ids": [sent]}),
    )
    .await
    .unwrap_or_else(|r| panic!("send rejected: {}", r.json));
    let res = delete(&app, &who, chat, sent).await;
    assert_category(&res, 409, "already_exists");
    assert_eq!(res.json["context"]["resource_name"], "attachment_locked");
    assert!(attachment_row(&app, chat, sent).await.deleted_at.is_none());
}

#[tokio::test]
async fn vector_store_creation_race_creates_one_store() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1", "file-doc2"]);
    script_vector_store(&app, VS_ID, Duration::from_millis(300), completed());

    let (a, b) = tokio::join!(
        upload(&app, &who, chat, "a.pdf", PDF, b"%PDF a"),
        upload(&app, &who, chat, "b.pdf", PDF, b"%PDF b"),
    );

    assert_eq!(
        (a.status.as_u16(), b.status.as_u16()),
        (201, 201),
        "{} / {}",
        a.json,
        b.json
    );
    assert_eq!(
        (&a.json["status"], &b.json["status"]),
        (&json!("ready"), &json!("ready"))
    );
    assert_eq!(vector_store_creates(&app), 1);
    let stores = vector_store_rows(&app, chat).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some(VS_ID));
    assert_eq!(
        app.gateway
            .requests_to(&Method::POST, &format!("/vector_stores/{VS_ID}/files"))
            .len(),
        2
    );
}

/// Every content part of type `input_image` in the request input.
fn input_images(request: &Value) -> Vec<Value> {
    request["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter(|part| part["type"] == "input_image")
        .cloned()
        .collect()
}

fn tool<'a>(request: &'a Value, kind: &str) -> Option<&'a Value> {
    request["tools"]
        .as_array()
        .and_then(|tools| tools.iter().find(|t| t["type"] == kind))
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one scenario across the tool kinds
async fn ready_attachments_reach_provider_tools() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_provider(&app, answer(&["ok"], 3, 2));
    let send = |body: Value| {
        let app = &app;
        let who = &who;
        async move {
            app.stream("POST", &stream_uri(chat), who, body)
                .await
                .unwrap_or_else(|r| panic!("send rejected: {}", r.json));
            provider_calls(app)
                .last()
                .cloned()
                .expect("a provider call")
        }
    };

    let req = send(json!({"content": "hi"})).await;
    assert!(req.get("tools").is_none(), "{req}");

    script_ready_documents(&app, &["file-pdf1"]);
    let pdf = id_of(&upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await);
    let req = send(json!({"content": "what does the report say"})).await;
    assert_eq!(
        tool(&req, "file_search"),
        Some(&json!({"type": "file_search", "vector_store_ids": [VS_ID], "max_num_results": 5})),
        "{req}"
    );
    assert!(
        req["instructions"]
            .as_str()
            .unwrap_or_default()
            .contains(DEFAULT_FILE_SEARCH_GUARD),
        "{req}"
    );

    script_files(&app, &["file-xlsx1"]);
    let xlsx = upload(&app, &who, chat, "d.xlsx", XLSX, b"PK").await;
    assert_eq!(xlsx.json["status"], "ready", "{}", xlsx.json);
    let req = send(json!({"content": "sum column A"})).await;
    let ci = tool(&req, "code_interpreter").unwrap_or_else(|| panic!("{req}"));
    assert_eq!(ci["container"]["file_ids"], json!(["file-xlsx1"]));
    assert_eq!(req["include"], json!(["code_interpreter_call.outputs"]));

    script_files(&app, &["file-img1"]);
    let image = id_of(&upload(&app, &who, chat, "p.png", "image/png", &png(8, 8)).await);
    let req = send(json!({"content": "describe", "attachment_ids": [image]})).await;
    assert_eq!(
        input_images(&req),
        vec![json!({"type": "input_image", "file_id": "file-img1"})],
        "{req}"
    );
    let req = send(json!({"content": "and now?"})).await;
    assert!(input_images(&req).is_empty(), "{req}");

    let req = send(json!({"content": "news", "web_search": {"enabled": true}})).await;
    assert_eq!(
        tool(&req, "web_search"),
        Some(&json!({"type": "web_search", "search_context_size": "low"})),
        "{req}"
    );
    assert!(
        req["instructions"]
            .as_str()
            .unwrap_or_default()
            .contains(DEFAULT_WEB_SEARCH_GUARD),
        "{req}"
    );

    assert_eq!(delete(&app, &who, chat, pdf).await.status, 204);
    let req = send(json!({"content": "again"})).await;
    assert!(tool(&req, "file_search").is_none(), "{req}");
    assert!(tool(&req, "code_interpreter").is_some(), "{req}");
}

// --- vector store creation protocol -------------------------------------------------------

#[tokio::test]
async fn stale_placeholder_is_reclaimed() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    let long_ago = db_now() - time::Duration::seconds(200);
    seed_vector_store_row_at(
        &app,
        who.subject_tenant_id(),
        chat,
        None,
        "openai",
        long_ago,
    )
    .await;
    script_ready_documents(&app, &["file-doc1"]);

    let res = upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await;

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "ready");
    assert_eq!(vector_store_creates(&app), 1);
    let stores = vector_store_rows(&app, chat).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some(VS_ID));
}

#[tokio::test]
async fn vector_store_creation_failure_fails_the_upload() {
    let app = TestApp::builder().build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    app.gateway.on(
        Method::POST,
        "/v1/vector_stores",
        Responder::json(500, json!({"error": {"message": "boom"}})),
    );
    script_file_deletes(&app);

    let res = upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await;

    assert_category(&res, 503, "service_unavailable");
    assert_eq!(res.headers["retry-after"], "10");
    assert!(
        vector_store_rows(&app, chat).await.is_empty(),
        "placeholder removed"
    );
    let row = &attachment_rows(&app, chat).await[0];
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    wait_for_file_delete(&app, "file-doc1").await;
}

#[tokio::test]
async fn loser_gives_up_when_the_placeholder_never_fills() {
    let app = TestApp::builder()
        .indexing_timings(fast_timings(Duration::from_secs(10)))
        .build()
        .await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    seed_vector_store_row(&app, who.subject_tenant_id(), chat, None, "openai").await;
    script_ready_documents(&app, &["file-doc1"]);
    script_file_deletes(&app);

    let res = upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await;

    assert_category(&res, 503, "service_unavailable");
    assert_eq!(
        vector_store_creates(&app),
        0,
        "a loser never creates the store"
    );
    let row = &attachment_rows(&app, chat).await[0];
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    assert_eq!(vector_store_rows(&app, chat).await[0].vector_store_id, None);
    wait_for_file_delete(&app, "file-doc1").await;
}

/// The winner's placeholder is reclaimed while it creates the store and another request sets
/// the chat's store: the CAS fails, the new store is deleted and the existing one is used.
#[tokio::test]
async fn lost_compare_and_set_uses_the_chat_store() {
    let app = Arc::new(TestApp::builder().build().await);
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    script_vector_store(&app, VS_ID, Duration::from_millis(300), completed());
    app.gateway.on(
        Method::POST,
        "/vector_stores/vs_existing/files",
        Responder::json(200, completed()),
    );
    app.gateway.on(
        Method::DELETE,
        &format!("/vector_stores/{VS_ID}"),
        Responder::json(200, json!({"deleted": true})),
    );
    let pending = tokio::spawn({
        let (app, who) = (Arc::clone(&app), who.clone());
        async move { upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await }
    });
    TestApp::wait_until("the winner creates the store", || async {
        vector_store_creates(&app) == 1
    })
    .await;
    delete_vector_store_rows(&app, chat).await;
    seed_vector_store_row(
        &app,
        who.subject_tenant_id(),
        chat,
        Some("vs_existing"),
        "openai",
    )
    .await;

    let res = pending.await.expect("upload task");

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "ready");
    assert_eq!(
        app.gateway
            .requests_to(&Method::POST, "/vector_stores/vs_existing/files")
            .len(),
        1
    );
    TestApp::wait_until("the superseded store is deleted", || async {
        !app.gateway
            .requests_to(&Method::DELETE, &format!("/vector_stores/{VS_ID}"))
            .is_empty()
    })
    .await;
    let stores = vector_store_rows(&app, chat).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs_existing"));
}

/// A store row of another backend that appears after the pre-check (during the provider
/// upload) is caught by the creation protocol.
#[tokio::test]
async fn provider_mismatch_after_the_provider_upload() {
    let app = Arc::new(TestApp::builder().build().await);
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    app.gateway.on_sequence(
        Method::POST,
        FILES_PATH,
        vec![Responder::Delayed(
            Duration::from_millis(300),
            Box::new(Responder::json(200, json!({"id": "file-doc1"}))),
        )],
    );
    script_vector_store(&app, VS_ID, Duration::ZERO, completed());
    script_file_deletes(&app);
    let pending = tokio::spawn({
        let (app, who) = (Arc::clone(&app), who.clone());
        async move { upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await }
    });
    wait_for_provider_upload(&app).await;
    seed_vector_store_row(
        &app,
        who.subject_tenant_id(),
        chat,
        Some("vs_other"),
        "other",
    )
    .await;

    let res = pending.await.expect("upload task");

    assert_category(&res, 409, "already_exists");
    assert_eq!(res.json["context"]["resource_name"], "provider_mismatch");
    let row = &attachment_rows(&app, chat).await[0];
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    assert_eq!(vector_store_creates(&app), 0);
    wait_for_file_delete(&app, "file-doc1").await;
}

// --- status reads ----------------------------------------------------------------------------

#[tokio::test]
async fn background_indexing_failure_enqueues_cleanup() {
    let app = TestApp::builder()
        .quiet_cleanup()
        .indexing_timings(fast_timings(Duration::from_secs(10)))
        .build()
        .await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, in_progress());
    script_index_status(&app, VS_ID, in_progress());
    let res = upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await;
    assert_eq!(res.json["status"], "uploaded", "{}", res.json);
    let att = id_of(&res);

    script_index_status(&app, VS_ID, json!({"status": "cancelled"}));

    wait_for_status(&app, &who, chat, att, "failed").await;
    let row = attachment_row(&app, chat, att).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    TestApp::wait_until("the cleanup event is delivered", || async {
        !app.outbox_payloads(CLEANUP_QUEUE).is_empty()
    })
    .await;
    let p = &app.outbox_payloads(CLEANUP_QUEUE)[0];
    assert_eq!(p["event_type"], "attachment_indexing_failed");
    assert_eq!(p["attachment_id"], att.to_string());
    assert_eq!(p["provider_file_id"], "file-doc1");
    assert!(
        app.gateway
            .requests_to(&Method::DELETE, "/v1/files/file-doc1")
            .is_empty(),
        "the outbox cleanup owns the delete"
    );
}

#[tokio::test]
async fn transient_status_errors_keep_polling() {
    let timings = IndexingTimings {
        request_deadline: Duration::from_secs(10),
        ..fast_timings(Duration::from_secs(10))
    };
    let app = TestApp::builder().indexing_timings(timings).build().await;
    let who = user();
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, in_progress());
    script_index_status(&app, VS_ID, completed());
    app.gateway.on_sequence(
        Method::GET,
        &format!("/vector_stores/{VS_ID}/files/"),
        vec![
            Responder::GatewayStatus(502),
            Responder::json(500, json!({"error": {"message": "try later"}})),
        ],
    );

    let res = upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await;

    assert_eq!(res.status, 201, "{}", res.json);
    assert_eq!(res.json["status"], "ready");
    assert_eq!(
        app.gateway
            .requests_to(
                &Method::GET,
                &format!("/vector_stores/{VS_ID}/files/file-doc1")
            )
            .len(),
        3
    );
}

// --- withdrawn uploads ---------------------------------------------------------------------

#[tokio::test]
async fn chat_deleted_during_the_upload_is_not_found() {
    let app = Arc::new(TestApp::builder().quiet_cleanup().build().await);
    let who = user();
    script_file_deletes(&app);

    // Deleted during the provider upload: the file id is never recorded and the file is
    // deleted.
    let chat = create_chat(&app, &who, None).await;
    app.gateway.on_sequence(
        Method::POST,
        FILES_PATH,
        vec![Responder::Delayed(
            Duration::from_millis(300),
            Box::new(Responder::json(200, json!({"id": "file-img1"}))),
        )],
    );
    let pending = tokio::spawn({
        let (app, who) = (Arc::clone(&app), who.clone());
        async move { upload(&app, &who, chat, "p.png", "image/png", &png(4, 4)).await }
    });
    wait_for_provider_upload(&app).await;
    let deleted = app
        .call("DELETE", &format!("{CHATS}/{chat}"), &who, None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.json);

    let res = pending.await.expect("upload task");

    assert_eq!(res.status, 404, "{}", res.json);
    assert_eq!(res.json["context"]["resource_type"], CHAT_RESOURCE);
    let row = &attachment_rows(&app, chat).await[0];
    assert_eq!(row.status, "pending");
    assert_eq!(row.provider_file_id, None);
    wait_for_file_delete(&app, "file-img1").await;

    // Deleted while the document is added to the vector store: the row keeps its file id for
    // the chat cleanup and never becomes ready.
    let chat = create_chat(&app, &who, None).await;
    script_files(&app, &["file-doc1"]);
    script_vector_store(&app, VS_ID, Duration::ZERO, completed());
    app.gateway.on(
        Method::POST,
        &format!("/vector_stores/{VS_ID}/files"),
        Responder::Delayed(
            Duration::from_millis(300),
            Box::new(Responder::json(200, completed())),
        ),
    );
    let pending = tokio::spawn({
        let (app, who) = (Arc::clone(&app), who.clone());
        async move { upload(&app, &who, chat, "r.pdf", PDF, b"%PDF").await }
    });
    TestApp::wait_until("the document is added to the store", || async {
        !app.gateway
            .requests_to(&Method::POST, &format!("/vector_stores/{VS_ID}/files"))
            .is_empty()
    })
    .await;
    let deleted = app
        .call("DELETE", &format!("{CHATS}/{chat}"), &who, None)
        .await;
    assert_eq!(deleted.status, 204, "{}", deleted.json);

    let res = pending.await.expect("upload task");

    assert_eq!(res.status, 404, "{}", res.json);
    assert_eq!(res.json["context"]["resource_type"], CHAT_RESOURCE);
    let row = &attachment_rows(&app, chat).await[0];
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.provider_file_id.as_deref(), Some("file-doc1"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
}

async fn wait_for_provider_upload(app: &TestApp) {
    TestApp::wait_until("the provider upload starts", || async {
        !app.gateway
            .requests_to(&Method::POST, FILES_PATH)
            .is_empty()
    })
    .await;
}

async fn wait_for_file_delete(app: &TestApp, file_id: &str) {
    TestApp::wait_until(&format!("provider file {file_id} is deleted"), || async {
        !app.gateway
            .requests_to(&Method::DELETE, &format!("/v1/files/{file_id}"))
            .is_empty()
    })
    .await;
}
