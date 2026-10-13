#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::case_sensitive_file_extension_comparisons
)]

//! Attachments API: upload (multipart, MIME / size / per-chat limits,
//! provider upload, vector-store creation protocol, indexing wait,
//! background indexing, thumbnails), get and delete (S§11, D "Upload
//! Attachment", D "File Upload", D§3.7 `chat_vector_stores`, ADR-0007).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use mini_chat::infra::db::entity::{attachment, chat_vector_store};
use mini_chat::infra::db::repos::{AttachmentRepo, ChatRepo, VectorStoreRepo};
use mini_chat_sdk::KillSwitches;
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use common::*;

const ATTACHMENT_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";
const CHAT_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";
const CLEANUP_QUEUE: &str = "mini-chat.attachment_cleanup";

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

fn id_of(body: &Value) -> Uuid {
    body["id"].as_str().unwrap().parse().unwrap()
}

async fn chat_row(
    app: &TestApp,
    tenant: Uuid,
    user: Uuid,
    chat: Uuid,
) -> mini_chat::infra::db::entity::chat::Model {
    ChatRepo
        .find_by_id(&app.db.conn().unwrap(), &tenant_scope(tenant, user), chat)
        .await
        .unwrap()
        .unwrap()
}

/// A multipart request whose body records whether it was read (and fails).
fn untouched_body_request(path: &str, polled: &Arc<AtomicBool>) -> Request<Body> {
    let flag = Arc::clone(polled);
    let stream = futures::stream::poll_fn(move |_| {
        flag.store(true, Ordering::SeqCst);
        Poll::Ready(Some(Err::<Bytes, _>(std::io::Error::other("body read"))))
    });
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "multipart/form-data; boundary=xyz")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn no_storage_calls(app: &TestApp) {
    let calls: Vec<_> = app
        .oagw
        .requests()
        .into_iter()
        .filter(|r| r.uri.contains("/files") || r.uri.contains("/vector_stores"))
        .collect();
    assert!(calls.is_empty(), "{calls:?}");
}

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_document_ready_201() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-doc1");
    script_vs_create(&app, "vs_chat1");
    script_add(&app, "vs_chat1", "file-doc1", "completed");

    let resp = upload(&client, chat, "Report Q3.pdf", PDF_CT, PDF_BYTES).await;

    let body = created(&resp);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["kind"], "document");
    assert_eq!(body["filename"], "Report Q3.pdf");
    assert_eq!(body["content_type"], PDF_CT);
    assert_eq!(body["size_bytes"], PDF_BYTES.len());
    assert!(body["created_at"].is_string(), "{body}");
    for key in [
        "error_code",
        "doc_summary",
        "img_thumbnail",
        "summary_updated_at",
    ] {
        assert!(body.get(key).is_none(), "{key} present: {body}");
    }
    let text = resp.text();
    assert!(!text.contains("file-") && !text.contains("vs_"), "{text}");

    let id = id_of(&body);
    let row = attachment_by_id(&app, id).await;
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.provider_file_id.as_deref(), Some("file-doc1"));
    assert_eq!(row.storage_backend, "openai");
    assert_eq!((row.tenant_id, row.chat_id), (tenant, chat));
    assert_eq!(row.uploaded_by_user_id, user);
    assert_eq!(row.attachment_kind, "document");
    let stores = vector_store_rows(&app).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs_chat1"));
    assert_eq!((stores[0].chat_id, stores[0].tenant_id), (chat, tenant));
    assert_eq!(stores[0].provider, "openai");

    let file = &requests_matching(&app, "POST", "/v1/files")[0];
    assert_eq!(file.uri, "/api.openai.com/v1/files");
    assert!(
        file.multipart_fields.iter().any(|(n, _, _)| n == "purpose"),
        "{file:?}"
    );
    assert!(String::from_utf8_lossy(&file.raw_body).contains("assistants"));
    assert!(
        file.multipart_fields.contains(&(
            "file".to_owned(),
            Some(format!("{chat}_{id}.pdf")),
            Some(PDF_CT.to_owned())
        )),
        "{:?}",
        file.multipart_fields
    );
    let create = requests_matching(&app, "POST", "/v1/vector_stores");
    assert_eq!(create[0].uri, "/api.openai.com/v1/vector_stores");
    assert_eq!(
        create[0].json_body.as_ref().unwrap()["name"],
        format!("chat-{chat}")
    );
    let add = &create[1];
    assert_eq!(add.uri, "/api.openai.com/v1/vector_stores/vs_chat1/files");
    let add_body = add.json_body.as_ref().unwrap();
    assert_eq!(add_body["file_id"], "file-doc1");
    assert_eq!(add_body["attributes"]["attachment_id"], id.to_string());
}

#[tokio::test]
async fn second_document_reuses_vector_store() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;

    upload_doc_ready(&app, &client, chat, "file-a1", "vs_one", true).await;
    upload_doc_ready(&app, &client, chat, "file-b2", "vs_one", false).await;

    assert_eq!(vs_creates(&app), 1);
    let adds = requests_matching(&app, "POST", "/v1/vector_stores/vs_one/files");
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[1].json_body.as_ref().unwrap()["file_id"], "file-b2");
    assert_eq!(vector_store_rows(&app).await.len(), 1);
}

#[tokio::test]
async fn indexing_polls_until_completed_within_deadline() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-p1");
    script_vs_create(&app, "vs_p");
    script_add(&app, "vs_p", "file-p1", "in_progress");
    script_status(&app, "vs_p", "file-p1", "in_progress");
    // A response without `status` counts as in progress.
    app.oagw.push_json(
        &status_path("vs_p", "file-p1"),
        200,
        json!({"id": "file-p1"}),
    );
    script_status(&app, "vs_p", "file-p1", "completed");

    let body = created(&upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await);

    assert_eq!(body["status"], "ready");
    assert_eq!(
        requests_matching(&app, "GET", "/vs_p/files/file-p1").len(),
        3
    );
}

#[tokio::test]
async fn indexing_still_running_returns_uploaded_then_ready() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-slow");
    script_vs_create(&app, "vs_s");
    script_add(&app, "vs_s", "file-slow", "in_progress");
    status_fallback(&app, "vs_s", "file-slow", "in_progress");

    let started = std::time::Instant::now();
    let body = created(&upload(&client, chat, "big.pdf", PDF_CT, PDF_BYTES).await);
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert_eq!(body["status"], "uploaded");
    assert!(body.get("error_code").is_none());
    let id = id_of(&body);
    assert_eq!(attachment_by_id(&app, id).await.status, "uploaded");

    // Each background round refreshes `updated_at` (upload reaper heartbeat).
    app.clock.advance(time::Duration::seconds(60));
    let expected = OffsetDateTime::from_unix_timestamp(CLOCK_START + 60).unwrap();
    wait_until(5, || async {
        attachment_by_id(&app, id).await.updated_at == expected
    })
    .await;
    assert_eq!(attachment_by_id(&app, id).await.status, "uploaded");

    script_status(&app, "vs_s", "file-slow", "completed");
    wait_until(5, || async {
        client.get(&attachment_path(chat, id)).await.json()["status"] == "ready"
    })
    .await;
    let row = attachment_by_id(&app, id).await;
    assert!(row.cleanup_status.is_none() && row.error_code.is_none());
}

#[tokio::test]
async fn indexing_failed_is_503_and_row_failed() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-bad");
    script_vs_create(&app, "vs_f");
    script_add(&app, "vs_f", "file-bad", "in_progress");
    script_status(&app, "vs_f", "file-bad", "failed");

    let resp = upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await;

    let body = expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.header("retry-after").as_deref(), Some("10"));
    assert!(!resp.text().contains("indexing_failed"), "{body}");
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert!(row.cleanup_status.is_none());
    let got = client.get(&attachment_path(chat, row.id)).await;
    assert_eq!(got.status, StatusCode::OK);
    let got = got.json();
    assert_eq!(got["status"], "failed");
    assert_eq!(got["error_code"], "indexing_failed");
    wait_until(5, || async {
        !requests_matching(&app, "DELETE", "/v1/files/file-bad").is_empty()
    })
    .await;

    // A rejected add to the vector store sets the same code.
    script_file(&app, "file-bad2");
    app.oagw.push_json(
        "/v1/vector_stores/vs_f/files",
        400,
        json!({"error": {"message": "bad file"}}),
    );
    let resp = upload(&client, chat, "b.pdf", PDF_CT, PDF_BYTES).await;
    expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    wait_until(5, || async {
        !requests_matching(&app, "DELETE", "/v1/files/file-bad2").is_empty()
    })
    .await;
}

#[tokio::test]
async fn status_poll_provider_429_fails_indexing() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-429");
    script_vs_create(&app, "vs_429");
    script_add(&app, "vs_429", "file-429", "in_progress");
    app.oagw.push_json(
        &status_path("vs_429", "file-429"),
        429,
        json!({"error": {"message": "rate limited"}}),
    );
    status_fallback(&app, "vs_429", "file-429", "completed");

    let resp = upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await;

    expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(
        requests_matching(&app, "GET", "/vs_429/files/file-429").len(),
        1
    );
}

#[tokio::test]
async fn background_indexing_failure_enqueues_cleanup() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-late");
    script_vs_create(&app, "vs_l");
    script_add(&app, "vs_l", "file-late", "in_progress");
    status_fallback(&app, "vs_l", "file-late", "in_progress");
    let body = created(&upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await);
    assert_eq!(body["status"], "uploaded");
    let id = id_of(&body);

    script_status(&app, "vs_l", "file-late", "failed");
    wait_until(5, || async {
        attachment_by_id(&app, id).await.status == "failed"
    })
    .await;

    let row = attachment_by_id(&app, id).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.deleted_at.is_none());
    // No inline delete: the outbox cleanup owns the provider file.
    assert!(requests_matching(&app, "DELETE", "/v1/files/").is_empty());
    let payloads = app.outbox_payloads(CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];
    assert_eq!(p["event_type"], "attachment_indexing_failed");
    assert_eq!(p["attachment_id"], id.to_string());
    assert_eq!(p["chat_id"], chat.to_string());
    assert_eq!(p["tenant_id"], tenant.to_string());
    assert_eq!(p["provider_file_id"], "file-late");
    assert_eq!(p["storage_backend"], "openai");
    assert_eq!(p["attachment_kind"], "document");
    assert_eq!(p["vector_store_id"], Value::Null);
    assert_eq!(p["secondary_ref"], Value::Null);
}

#[tokio::test]
async fn background_indexing_timeout_fails_with_cleanup() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-never");
    script_vs_create(&app, "vs_n");
    script_add(&app, "vs_n", "file-never", "in_progress");
    status_fallback(&app, "vs_n", "file-never", "in_progress");
    let id = id_of(&created(
        &upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await,
    ));

    // Background limit of `fast_timings` is 1.5 s.
    wait_until(5, || async {
        attachment_by_id(&app, id).await.status == "failed"
    })
    .await;

    let row = attachment_by_id(&app, id).await;
    assert_eq!(row.error_code.as_deref(), Some("indexing_failed"));
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let payloads = app.outbox_payloads(CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0]["event_type"], "attachment_indexing_failed");
}

#[tokio::test]
async fn background_indexing_stops_on_shutdown() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-stop");
    script_vs_create(&app, "vs_st");
    script_add(&app, "vs_st", "file-stop", "in_progress");
    status_fallback(&app, "vs_st", "file-stop", "in_progress");
    let id = id_of(&created(
        &upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await,
    ));

    app.services.shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;
    status_fallback(&app, "vs_st", "file-stop", "completed");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let row = attachment_by_id(&app, id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.error_code.is_none() && row.cleanup_status.is_none());
}

#[tokio::test]
async fn deleted_attachment_stops_background_indexing() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-del");
    script_vs_create(&app, "vs_d");
    script_add(&app, "vs_d", "file-del", "in_progress");
    status_fallback(&app, "vs_d", "file-del", "in_progress");
    let id = id_of(&created(
        &upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await,
    ));

    let resp = client.delete(&attachment_path(chat, id)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    status_fallback(&app, "vs_d", "file-del", "completed");
    tokio::time::sleep(Duration::from_millis(300)).await;

    let row = attachment_by_id(&app, id).await;
    assert_eq!(row.status, "uploaded");
    assert!(row.deleted_at.is_some());
    assert!(row.error_code.is_none());
}

#[tokio::test]
async fn chat_deletion_stops_background_indexing() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-cd");
    script_vs_create(&app, "vs_cd");
    script_add(&app, "vs_cd", "file-cd", "in_progress");
    status_fallback(&app, "vs_cd", "file-cd", "in_progress");
    let id = id_of(&created(
        &upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await,
    ));

    let resp = client.delete(&chat_path(chat)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    status_fallback(&app, "vs_cd", "file-cd", "completed");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Chat cleanup owns the provider file; the row never becomes ready.
    let row = attachment_by_id(&app, id).await;
    assert_eq!(row.status, "uploaded");
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert!(row.error_code.is_none());
}

#[tokio::test]
async fn provider_upload_failure_is_503_upload_failed() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.oagw
        .push_json(FILES_PATH, 500, json!({"error": {"message": "boom"}}));

    let resp = upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await;

    expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.header("retry-after").as_deref(), Some("10"));
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_failed"));
    assert!(row.provider_file_id.is_none());
    let got = client.get(&attachment_path(chat, row.id)).await.json();
    assert_eq!(got["status"], "failed");
    assert_eq!(got["error_code"], "upload_failed");
    assert_eq!(vs_creates(&app), 0);
}

// ---------------------------------------------------------------------------
// Vector store creation protocol
// ---------------------------------------------------------------------------

fn placeholder(
    chat: &mini_chat::infra::db::entity::chat::Model,
    age_secs: i64,
) -> chat_vector_store::Model {
    chat_vector_store::Model {
        id: Uuid::new_v4(),
        tenant_id: chat.tenant_id,
        chat_id: chat.id,
        vector_store_id: None,
        provider: "openai".to_owned(),
        file_count: 0,
        created_at: OffsetDateTime::from_unix_timestamp(CLOCK_START - age_secs).unwrap(),
    }
}

#[tokio::test]
async fn stale_vector_store_placeholder_is_reclaimed() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_row(&app, tenant, user, chat_id).await;
    VectorStoreRepo
        .insert(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            placeholder(&chat, 121),
        )
        .await
        .unwrap();

    upload_doc_ready(&app, &client, chat_id, "file-r1", "vs_new", true).await;

    assert_eq!(vs_creates(&app), 1);
    let stores = vector_store_rows(&app).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].vector_store_id.as_deref(), Some("vs_new"));
}

#[tokio::test]
async fn fresh_placeholder_loser_polls_then_503() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_row(&app, tenant, user, chat_id).await;
    VectorStoreRepo
        .insert(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            placeholder(&chat, 30),
        )
        .await
        .unwrap();
    script_file(&app, "file-loser");

    let resp = upload(&client, chat_id, "a.pdf", PDF_CT, PDF_BYTES).await;

    expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.header("retry-after").as_deref(), Some("10"));
    assert_eq!(vs_creates(&app), 0, "the loser never creates a store");
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));
    wait_until(5, || async {
        !requests_matching(&app, "DELETE", "/v1/files/file-loser").is_empty()
    })
    .await;
    // The other request's placeholder is left alone.
    assert_eq!(vector_store_rows(&app).await.len(), 1);
}

#[tokio::test]
async fn loser_uses_store_set_by_winner() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_row(&app, tenant, user, chat_id).await;
    let row = placeholder(&chat, 0);
    let row_id = row.id;
    VectorStoreRepo
        .insert(&app.db.conn().unwrap(), &tenant_scope(tenant, user), row)
        .await
        .unwrap();
    script_file(&app, "file-w");
    script_add(&app, "vs_winner", "file-w", "completed");
    // The concurrent winner writes through the app's pool (the test DB's
    // single connection), like another upload request would.
    let db = Arc::clone(&app.db);
    let winner = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(15)).await;
        let set = VectorStoreRepo
            .set_vector_store_id(
                &db.conn().unwrap(),
                &tenant_scope(tenant, user),
                chat_id,
                row_id,
                "vs_winner",
            )
            .await
            .unwrap();
        assert_eq!(set, 1);
    });

    let body = created(&upload(&client, chat_id, "a.pdf", PDF_CT, PDF_BYTES).await);
    winner.await.unwrap();

    assert_eq!(body["status"], "ready");
    assert_eq!(vs_creates(&app), 0);
    assert_eq!(
        requests_matching(&app, "POST", "/v1/vector_stores/vs_winner/files").len(),
        1
    );
}

#[tokio::test]
async fn vector_store_create_failure_is_503_and_placeholder_removed() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-vsf");
    app.oagw
        .push_json(VS_PATH, 500, json!({"error": {"message": "down"}}));

    let resp = upload(&client, chat, "a.pdf", PDF_CT, PDF_BYTES).await;

    expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    assert!(vector_store_rows(&app).await.is_empty());
    let row = attachment_rows(&app).await.pop().unwrap();
    assert_eq!(row.error_code.as_deref(), Some("vector_store_failed"));

    // The next upload starts over and creates the store.
    upload_doc_ready(&app, &client, chat, "file-ok", "vs_ok", true).await;
    assert_eq!(vs_creates(&app), 2);
}

#[tokio::test]
async fn provider_mismatch_409() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_row(&app, tenant, user, chat_id).await;
    let mut row = placeholder(&chat, 0);
    row.vector_store_id = Some("vs_other".to_owned());
    "azure_other".clone_into(&mut row.provider);
    VectorStoreRepo
        .insert(&app.db.conn().unwrap(), &tenant_scope(tenant, user), row)
        .await
        .unwrap();

    let resp = upload(&client, chat_id, "a.pdf", PDF_CT, PDF_BYTES).await;

    let body = expect_problem(&resp, StatusCode::CONFLICT);
    assert_eq!(body["context"]["resource_name"], "provider_mismatch");
    assert!(requests_matching(&app, "POST", "/v1/files").is_empty());
    assert!(attachment_rows(&app).await.is_empty());
    // Images do not use the vector store.
    upload_image_ready(&app, &client, chat_id, "file-img").await;
}

// ---------------------------------------------------------------------------
// Images and code-interpreter files
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_image_ready_with_thumbnail() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-img1");

    let body = created(&upload(&client, chat, "photo.png", "image/png", &png(300, 200)).await);

    assert_eq!(body["status"], "ready");
    assert_eq!(body["kind"], "image");
    assert_eq!(body["content_type"], "image/png");
    let thumb = &body["img_thumbnail"];
    assert_eq!(thumb["content_type"], "image/webp");
    assert_eq!(thumb["width"], 128);
    assert_eq!(thumb["height"], 85);
    let webp = base64::engine::general_purpose::STANDARD
        .decode(thumb["data_base64"].as_str().unwrap())
        .unwrap();
    let img = image::load_from_memory_with_format(&webp, image::ImageFormat::WebP).unwrap();
    assert_eq!((img.width(), img.height()), (128, 85));

    let id = id_of(&body);
    let row = attachment_by_id(&app, id).await;
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert_eq!(
        (row.img_thumbnail_width, row.img_thumbnail_height),
        (Some(128), Some(85))
    );
    assert!(
        app.oagw
            .requests()
            .iter()
            .all(|r| !r.uri.contains("vector_stores"))
    );
    let file = &requests_matching(&app, "POST", "/v1/files")[0];
    assert!(file.multipart_fields.contains(&(
        "file".to_owned(),
        Some(format!("{chat}_{id}.png")),
        Some("image/png".to_owned())
    )));
    // GET returns the same thumbnail.
    let got = client.get(&attachment_path(chat, id)).await.json();
    assert_eq!(got["img_thumbnail"], *thumb);
}

#[tokio::test]
async fn undecodable_image_is_ready_without_thumbnail() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-junk");

    let body = created(&upload(&client, chat, "x.png", "image/png", b"not really a png").await);

    assert_eq!(body["status"], "ready");
    assert!(body.get("img_thumbnail").is_none(), "{body}");
    assert!(body.get("error_code").is_none(), "{body}");
}

#[tokio::test]
async fn xlsx_ready_for_code_interpreter_no_vector_store() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-x1");

    let body = created(&upload(&client, chat, "sheet.xlsx", XLSX_CT, XLSX_BYTES).await);

    assert_eq!(body["status"], "ready");
    assert_eq!(body["kind"], "document");
    let row = attachment_by_id(&app, id_of(&body)).await;
    assert!(row.for_code_interpreter && !row.for_file_search);
    assert_eq!(row.provider_file_id.as_deref(), Some("file-x1"));
    assert!(
        app.oagw
            .requests()
            .iter()
            .all(|r| !r.uri.contains("vector_stores"))
    );
    assert!(vector_store_rows(&app).await.is_empty());
}

#[tokio::test]
async fn xlsx_rejected_when_code_interpreter_unavailable() {
    let check = |resp: &TestResponse| {
        let body = expect_problem(resp, StatusCode::BAD_REQUEST);
        assert_eq!(
            violation(&body),
            ("file".to_owned(), "CODE_INTERPRETER_UNAVAILABLE".to_owned())
        );
        assert_eq!(body["context"]["resource_type"], ATTACHMENT_TYPE);
    };

    let app = TestApp::builder()
        .kill_switches(KillSwitches {
            disable_code_interpreter: true,
            ..KillSwitches::default()
        })
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    check(&upload(&client, chat, "sheet.xlsx", XLSX_CT, XLSX_BYTES).await);
    no_storage_calls(&app);
    assert!(attachment_rows(&app).await.is_empty());

    let mut no_ci = standard_model("s-noci");
    no_ci.general_config.tool_support.code_interpreter = false;
    let app = TestApp::builder().catalog(vec![no_ci]).build().await;
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s-noci").await;
    check(&upload(&client, chat, "sheet.xlsx", XLSX_CT, XLSX_BYTES).await);
    no_storage_calls(&app);
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

fn assert_too_large(resp: &TestResponse) {
    let body = expect_problem(resp, StatusCode::BAD_REQUEST);
    assert_eq!(
        violation(&body),
        ("content_length".to_owned(), "FILE_TOO_LARGE".to_owned())
    );
}

#[tokio::test]
async fn document_size_limit_is_min_of_rag_and_model() {
    // Gear limit 1 KiB.
    let app = TestApp::builder()
        .config(|c| c.rag.uploaded_file_max_size_kb = 1)
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    assert_too_large(&upload(&client, chat, "a.pdf", PDF_CT, &[b'x'; 1025]).await);
    no_storage_calls(&app);
    assert!(attachment_rows(&app).await.is_empty());

    // Model limit 1 MiB below the gear limit (25 MiB).
    let mut small = standard_model("s-small");
    small.general_config.max_file_size_mb = 1;
    let app = TestApp::builder().catalog(vec![small]).build().await;
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s-small").await;
    assert_too_large(&upload(&client, chat, "a.pdf", PDF_CT, &vec![b'x'; 1_048_577]).await);
    script_file(&app, "file-1m");
    script_vs_create(&app, "vs_1m");
    script_add(&app, "vs_1m", "file-1m", "completed");
    let body = created(&upload(&client, chat, "a.pdf", PDF_CT, &vec![b'x'; 1_048_576]).await);
    assert_eq!(body["size_bytes"], 1_048_576);
}

#[tokio::test]
async fn model_max_file_size_zero_means_no_model_cap() {
    // max_file_size_mb = 0 (absent in the catalog): only the gear limit applies.
    let mut uncapped = standard_model("s-uncapped");
    uncapped.general_config.max_file_size_mb = 0;
    let app = TestApp::builder()
        .config(|c| c.rag.uploaded_file_max_size_kb = 1)
        .catalog(vec![uncapped])
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s-uncapped").await;
    assert_too_large(&upload(&client, chat, "a.pdf", PDF_CT, &[b'x'; 1025]).await);
    script_file(&app, "file-1k");
    script_vs_create(&app, "vs_1k");
    script_add(&app, "vs_1k", "file-1k", "completed");
    let body = created(&upload(&client, chat, "a.pdf", PDF_CT, &[b'x'; 1024]).await);
    assert_eq!(body["size_bytes"], 1024);
}

#[tokio::test]
async fn image_size_limit_is_kind_specific() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;

    assert_too_large(
        &upload(
            &client,
            chat,
            "big.png",
            "image/png",
            &vec![0; 5120 * 1024 + 1],
        )
        .await,
    );

    no_storage_calls(&app);
}

#[tokio::test]
async fn unsupported_content_type_is_400() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;

    for (name, ct) in [
        ("a.zip", "application/zip"),
        ("blob.bin", "application/octet-stream"),
    ] {
        let body = expect_problem(
            &upload(&client, chat, name, ct, b"PK").await,
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(
            violation(&body),
            (
                "content_type".to_owned(),
                "UNSUPPORTED_CONTENT_TYPE".to_owned()
            )
        );
    }
    no_storage_calls(&app);
    assert!(attachment_rows(&app).await.is_empty());
}

#[tokio::test]
async fn octet_stream_pdf_is_accepted_as_pdf() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-o");
    script_vs_create(&app, "vs_o");
    script_add(&app, "vs_o", "file-o", "completed");

    let body = created(
        &upload(
            &client,
            chat,
            "report.pdf",
            "application/octet-stream",
            PDF_BYTES,
        )
        .await,
    );

    assert_eq!(body["content_type"], PDF_CT);
    assert_eq!(
        attachment_by_id(&app, id_of(&body)).await.content_type,
        PDF_CT
    );
    let file = &requests_matching(&app, "POST", "/v1/files")[0];
    assert_eq!(file.multipart_fields[1].2.as_deref(), Some(PDF_CT));
}

#[tokio::test]
async fn csv_is_stored_as_text_plain() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    script_file(&app, "file-csv");
    script_vs_create(&app, "vs_c");
    script_add(&app, "vs_c", "file-csv", "completed");

    let body = created(&upload(&client, chat, "data.csv", "text/csv", b"a,b\n1,2\n").await);

    assert_eq!(body["content_type"], "text/plain");
    assert_eq!(body["filename"], "data.csv");
    let id = id_of(&body);
    let row = attachment_by_id(&app, id).await;
    assert_eq!(row.content_type, "text/plain");
    assert!(row.for_file_search);
    let file = &requests_matching(&app, "POST", "/v1/files")[0];
    assert!(file.multipart_fields.contains(&(
        "file".to_owned(),
        Some(format!("{chat}_{id}.txt")),
        Some("text/plain".to_owned())
    )));
}

#[tokio::test]
async fn per_chat_document_limit_429() {
    let app = TestApp::builder()
        .config(|c| c.rag.max_documents_per_chat = 1)
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    upload_doc_ready(&app, &client, chat, "file-one", "vs_dl", true).await;

    let resp = upload(&client, chat, "two.pdf", PDF_CT, PDF_BYTES).await;

    let body = expect_problem(&resp, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body["context"]["violations"][0]["subject"],
        "document_limit"
    );
    assert_eq!(requests_matching(&app, "POST", "/v1/files").len(), 1);
    assert_eq!(attachment_rows(&app).await.len(), 1);
    // Images are not documents.
    upload_image_ready(&app, &client, chat, "file-img").await;
}

#[tokio::test]
async fn per_chat_storage_limit_429_ignores_failed() {
    let app = TestApp::builder()
        .config(|c| c.rag.max_total_upload_mb_per_chat = 1)
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let big = vec![b'x'; 600 * 1024];
    // A failed upload does not count.
    app.oagw
        .push_json(FILES_PATH, 500, json!({"error": {"message": "boom"}}));
    expect_problem(
        &upload(&client, chat, "a.pdf", PDF_CT, &big).await,
        StatusCode::SERVICE_UNAVAILABLE,
    );
    script_file(&app, "file-b");
    script_vs_create(&app, "vs_b");
    script_add(&app, "vs_b", "file-b", "completed");
    created(&upload(&client, chat, "b.pdf", PDF_CT, &big).await);

    let resp = upload(&client, chat, "c.png", "image/png", &big).await;

    let body = expect_problem(&resp, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["context"]["violations"][0]["subject"], "storage_limit");
    assert_eq!(attachment_rows(&app).await.len(), 2);
}

#[tokio::test]
async fn disable_images_rejects_image_upload() {
    let app = TestApp::builder()
        .kill_switches(KillSwitches {
            disable_images: true,
            ..KillSwitches::default()
        })
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;

    let resp = upload(&client, chat, "p.png", "image/png", &png(4, 4)).await;

    let body = expect_problem(&resp, StatusCode::BAD_REQUEST);
    let v = &body["context"]["violations"][0];
    assert_eq!(v["subject"], "images");
    assert_eq!(v["type"], "FEATURE_DISABLED");
    no_storage_calls(&app);
    assert!(attachment_rows(&app).await.is_empty());
}

#[tokio::test]
async fn upload_concurrency_limit_503() {
    let app = TestApp::builder()
        .config(|c| c.rag.max_concurrent_uploads = 1)
        .build()
        .await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let held = app
        .services
        .attachments
        .prepare_upload(&client.ctx, chat)
        .await
        .unwrap();

    let resp = upload(&client, chat, "p.png", "image/png", &png(4, 4)).await;

    expect_problem(&resp, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(resp.header("retry-after").as_deref(), Some("5"));
    no_storage_calls(&app);
    drop(held);
    upload_image_ready(&app, &client, chat, "file-after").await;
}

// ---------------------------------------------------------------------------
// Multipart and filenames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multipart_errors() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let raw = |ct: &str, body: &'static str| {
        Request::builder()
            .method(Method::POST)
            .uri(attachments_path(chat))
            .header(header::CONTENT_TYPE, ct)
            .body(Body::from(body))
            .unwrap()
    };
    let reason = |resp: &TestResponse| violation(&expect_problem(resp, StatusCode::BAD_REQUEST));

    let resp = client.send(raw("multipart/form-data", "x")).await;
    assert_eq!(
        reason(&resp),
        ("content_type".into(), "BOUNDARY_REQUIRED".into())
    );

    let resp = client
        .send(raw(
            "multipart/form-data; boundary=abc",
            "garbage, not multipart",
        ))
        .await;
    assert_eq!(
        reason(&resp),
        ("multipart".into(), "MULTIPART_ERROR".into())
    );

    let resp = client
        .post_multipart(
            &attachments_path(chat),
            vec![MultipartPart {
                name: "other".into(),
                filename: None,
                content_type: None,
                data: b"x".to_vec(),
            }],
        )
        .await;
    assert_eq!(reason(&resp), ("file".into(), "MISSING_FILE".into()));

    let resp = client
        .post_multipart(
            &attachments_path(chat),
            vec![MultipartPart {
                name: "file".into(),
                filename: Some("a.pdf".into()),
                content_type: None,
                data: PDF_BYTES.to_vec(),
            }],
        )
        .await;
    assert_eq!(
        reason(&resp),
        ("content_type".into(), "MISSING_CONTENT_TYPE".into())
    );

    no_storage_calls(&app);
    assert!(attachment_rows(&app).await.is_empty());
}

#[tokio::test]
async fn filename_rules() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;

    // Missing filename -> "upload" (the provider name uses the MIME extension).
    script_file(&app, "file-n1");
    script_vs_create(&app, "vs_fn");
    script_add(&app, "vs_fn", "file-n1", "completed");
    let resp = client
        .post_multipart(
            &attachments_path(chat),
            vec![MultipartPart {
                name: "file".into(),
                filename: None,
                content_type: Some(PDF_CT.into()),
                data: PDF_BYTES.to_vec(),
            }],
        )
        .await;
    let body = created(&resp);
    assert_eq!(body["filename"], "upload");
    let id = id_of(&body);
    assert_eq!(
        requests_matching(&app, "POST", "/v1/files")[0].multipart_fields[1].1,
        Some(format!("{chat}_{id}.pdf"))
    );

    // Path separators are stripped.
    script_file(&app, "file-n2");
    script_add(&app, "vs_fn", "file-n2", "completed");
    let body = created(&upload(&client, chat, "../../etc/passwd.txt", "text/plain", b"hi").await);
    assert_eq!(body["filename"], "passwd.txt");
    assert_eq!(
        attachment_by_id(&app, id_of(&body)).await.filename,
        "passwd.txt"
    );

    // Longer than 255 characters: truncated, extension kept.
    script_file(&app, "file-n3");
    script_add(&app, "vs_fn", "file-n3", "completed");
    let long = format!("{}.pdf", "a".repeat(300));
    let body = created(&upload(&client, chat, &long, PDF_CT, PDF_BYTES).await);
    let name = body["filename"].as_str().unwrap();
    assert_eq!(name.chars().count(), 255);
    assert!(name.ends_with(".pdf"), "{name}");
    let id = id_of(&body);
    assert_eq!(
        requests_matching(&app, "POST", "/v1/files")[2].multipart_fields[1].1,
        Some(format!("{chat}_{id}.pdf"))
    );
}

// ---------------------------------------------------------------------------
// Checks before the body is read
// ---------------------------------------------------------------------------

#[tokio::test]
async fn upload_to_unknown_chat_404_chat_before_body() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);

    let polled = Arc::new(AtomicBool::new(false));
    let resp = client
        .send(untouched_body_request(
            &attachments_path(Uuid::new_v4()),
            &polled,
        ))
        .await;

    let body = expect_problem(&resp, StatusCode::NOT_FOUND);
    assert_eq!(body["context"]["resource_type"], CHAT_TYPE);
    assert!(
        !polled.load(Ordering::SeqCst),
        "body was read before the chat check"
    );

    // Another user's chat.
    let other = app.as_user(Uuid::new_v4(), tenant);
    let chat = create_chat(&other, "s1").await;
    let resp = client
        .send(untouched_body_request(&attachments_path(chat), &polled))
        .await;
    expect_problem(&resp, StatusCode::NOT_FOUND);
    assert!(!polled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn chat_model_removed_is_400_invalid_model() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    app.policy.set_catalog(vec![premium_model("p1")]);

    let polled = Arc::new(AtomicBool::new(false));
    let resp = client
        .send(untouched_body_request(&attachments_path(chat), &polled))
        .await;

    let body = expect_problem(&resp, StatusCode::BAD_REQUEST);
    assert_eq!(violation(&body), ("model".into(), "INVALID_MODEL".into()));
    assert!(
        !polled.load(Ordering::SeqCst),
        "body was read before the model check"
    );
}

// ---------------------------------------------------------------------------
// Get / delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_delete_rules() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;
    let chat = chat_row(&app, tenant, user, chat_id).await;

    // Another user's attachment in my chat, an attachment of my other chat,
    // an unknown id: all 404 with the attachment type.
    let foreign = attachment::Model {
        uploaded_by_user_id: Uuid::new_v4(),
        status: "ready".to_owned(),
        ..attachment_row(&chat)
    };
    let foreign_id = foreign.id;
    AttachmentRepo
        .insert(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            foreign,
        )
        .await
        .unwrap();
    let other_chat = create_chat(&client, "s1").await;
    let elsewhere = upload_image_ready(&app, &client, other_chat, "file-else").await;
    for id in [foreign_id, elsewhere, Uuid::new_v4()] {
        for resp in [
            client.get(&attachment_path(chat_id, id)).await,
            client.delete(&attachment_path(chat_id, id)).await,
        ] {
            let body = expect_problem(&resp, StatusCode::NOT_FOUND);
            assert_eq!(body["context"]["resource_type"], ATTACHMENT_TYPE, "{body}");
        }
    }
    assert!(
        attachment_by_id(&app, foreign_id)
            .await
            .deleted_at
            .is_none()
    );

    // Own unreferenced attachment: GET, then DELETE -> 204 + cleanup event.
    let img = upload_image_ready(&app, &client, chat_id, "file-mine").await;
    let got = client.get(&attachment_path(chat_id, img)).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.json()["id"], img.to_string());
    let resp = client.delete(&attachment_path(chat_id, img)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());
    let row = attachment_by_id(&app, img).await;
    assert!(row.deleted_at.is_some());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let gone = client.get(&attachment_path(chat_id, img)).await;
    expect_problem(&gone, StatusCode::NOT_FOUND);
    // Idempotent re-delete: 204 without a second event.
    let resp = client.delete(&attachment_path(chat_id, img)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT);

    // An attachment referenced by a sent message is locked.
    let doc = upload_doc_ready(&app, &client, chat_id, "file-ref", "vs_ref", true).await;
    push_hello(&app);
    let sent = client
        .post_json(
            &stream_path(chat_id),
            &json!({"content": "see file", "attachment_ids": [doc]}),
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    let resp = client.delete(&attachment_path(chat_id, doc)).await;
    let body = expect_problem(&resp, StatusCode::CONFLICT);
    assert_eq!(body["context"]["resource_name"], "attachment_locked");
    assert!(attachment_by_id(&app, doc).await.deleted_at.is_none());

    let payloads = app.outbox_payloads(CLEANUP_QUEUE).await;
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    let p = &payloads[0];
    assert_eq!(p["event_type"], "attachment_deleted");
    assert_eq!(p["attachment_id"], img.to_string());
    assert_eq!(p["chat_id"], chat_id.to_string());
    assert_eq!(p["tenant_id"], tenant.to_string());
    assert_eq!(p["provider_file_id"], "file-mine");
    assert_eq!(p["storage_backend"], "openai");
    assert_eq!(p["attachment_kind"], "image");
    assert_eq!(p["vector_store_id"], Value::Null);
    assert_eq!(p["secondary_ref"], Value::Null);
}

#[tokio::test]
async fn routes_match_openapi_operation_ids() {
    let app = TestApp::builder().build().await;
    let ops: Vec<(String, String)> = app
        .operations()
        .into_iter()
        .filter(|o| o.path.contains("/attachments"))
        .map(|o| (o.method.to_string(), o.operation_id.unwrap_or_default()))
        .collect();
    for expected in [
        ("POST", "mini_chat.upload_attachment"),
        ("GET", "mini_chat.get_attachment"),
        ("DELETE", "mini_chat.delete_attachment"),
    ] {
        assert!(
            ops.iter()
                .any(|(m, id)| m == expected.0 && id == expected.1),
            "{expected:?} not in {ops:?}"
        );
    }
}
