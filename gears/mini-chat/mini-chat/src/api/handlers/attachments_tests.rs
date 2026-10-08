//! Router-level tests of the attachment endpoints (upload / get / delete).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use base64::Engine as _;
use http::StatusCode;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::attachments::indexing::{self, IndexingTimings};
use crate::domain::attachments::test_support::{
    BOUNDARY, file_deletes, insert_message_link, insert_row, multipart_body, png, requests, row, row_template, rows,
    upload, upload_doc, upload_raw, vector_store_rows,
};
use crate::testing::{NO_VISION, STANDARD, TENANT_A, TestApp, USER_A1, USER_A2, ctx, ctx_a1};

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

fn reason(json: &Value) -> &str {
    json["context"]["field_violations"][0]["reason"].as_str().unwrap_or_default()
}

fn field(json: &Value) -> &str {
    json["context"]["field_violations"][0]["field"].as_str().unwrap_or_default()
}

fn att_uri(chat: Uuid, att: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments/{att}")
}

// ───────────────────────────── upload: success paths ─────────────────────────────

#[tokio::test]
async fn document_upload_is_ready_and_indexed() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "report.pdf", "application/pdf", b"%PDF-1.4 body").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "ready");
    assert_eq!(json["kind"], "document");
    assert_eq!(json["filename"], "report.pdf");
    assert_eq!(json["content_type"], "application/pdf");
    assert_eq!(json["size_bytes"], 13);
    for absent in ["error_code", "doc_summary", "img_thumbnail", "summary_updated_at", "provider_file_id", "storage_backend"] {
        assert!(json.get(absent).is_none(), "{absent} must be omitted: {json}");
    }
    assert!(json["created_at"].as_str().is_some());
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();

    let uploads = requests(&t, "POST", "/v1/files");
    assert_eq!(uploads.len(), 1);
    let parts = &uploads[0].multipart;
    assert!(parts.iter().any(|p| p.0 == "purpose"));
    let file_part = parts.iter().find(|p| p.0 == "file").expect("file part");
    assert_eq!(file_part.1.as_deref(), Some(format!("{chat}_{id}.pdf").as_str()));
    assert_eq!(file_part.2.as_deref(), Some("application/pdf"));
    assert_eq!(requests(&t, "POST", "/v1/vector_stores").iter().filter(|r| !r.uri.contains("/files")).count(), 1);
    let adds: Vec<_> = requests(&t, "POST", "/files").into_iter().filter(|r| r.uri.contains("/vector_stores/")).collect();
    assert_eq!(adds.len(), 1);
    assert_eq!(adds[0].json.as_ref().unwrap()["attributes"]["attachment_id"], id.to_string());

    let r = row(&t, id).await;
    assert_eq!(r.status, "ready");
    assert!(r.for_file_search && !r.for_code_interpreter);
    assert!(r.provider_file_id.is_some());
    assert_eq!(r.storage_backend, "openai");
    assert_eq!(r.uploaded_by_user_id, USER_A1);
    let vs = vector_store_rows(&t, chat).await;
    assert_eq!(vs.len(), 1);
    assert!(vs[0].vector_store_id.is_some());
    assert_eq!(vs[0].provider, "openai");
}

#[tokio::test]
async fn second_document_reuses_the_chat_vector_store() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    upload_doc(&t, &c, chat).await;
    upload_doc(&t, &c, chat).await;
    let creates = requests(&t, "POST", "/v1/vector_stores").into_iter().filter(|r| !r.uri.contains("/files")).count();
    assert_eq!(creates, 1);
    assert_eq!(vector_store_rows(&t, chat).await.len(), 1);
    // A different chat gets its own store.
    let chat2 = t.create_chat(&c, None).await;
    upload_doc(&t, &c, chat2).await;
    let creates = requests(&t, "POST", "/v1/vector_stores").into_iter().filter(|r| !r.uri.contains("/files")).count();
    assert_eq!(creates, 2);
}

#[tokio::test]
async fn image_upload_gets_a_webp_thumbnail() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "photo.png", "image/png", &png(300, 200)).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["kind"], "image");
    assert_eq!(json["status"], "ready");
    let th = &json["img_thumbnail"];
    assert_eq!(th["content_type"], "image/webp");
    assert_eq!((th["width"].as_i64().unwrap(), th["height"].as_i64().unwrap()), (128, 85));
    let bytes = base64::engine::general_purpose::STANDARD.decode(th["data_base64"].as_str().unwrap()).unwrap();
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP).unwrap();
    assert!(img.width() <= 128 && img.height() <= 128);
    assert!(requests(&t, "POST", "/vector_stores").is_empty(), "images are not indexed");
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    let r = row(&t, id).await;
    assert!(!r.for_file_search && !r.for_code_interpreter);
    // GET returns the same thumbnail.
    let (st, _, got) = t.call(&c, "GET", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["img_thumbnail"], json["img_thumbnail"]);
}

#[tokio::test]
async fn undecodable_image_is_ready_without_thumbnail() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "broken.png", "image/png", b"not really a png").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "ready");
    assert!(json.get("img_thumbnail").is_none());
    assert!(json.get("error_code").is_none());
}

#[tokio::test]
async fn xlsx_is_routed_to_code_interpreter() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "data.xlsx", XLSX, b"PK fake xlsx").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "ready");
    assert_eq!(json["kind"], "document");
    let r = row(&t, json["id"].as_str().unwrap().parse().unwrap()).await;
    assert!(r.for_code_interpreter && !r.for_file_search);
    assert!(requests(&t, "POST", "/vector_stores").is_empty());
}

#[tokio::test]
async fn xlsx_rejected_when_code_interpreter_unavailable() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    // Model without code interpreter support.
    let chat = t.create_chat(&c, Some(NO_VISION)).await;
    let (st, _, json) = upload(&t, &c, chat, "data.xlsx", XLSX, b"PK").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(reason(&json), "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(json["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
    // Kill switch.
    let chat = t.create_chat(&c, None).await;
    t.policy.with_snapshot(|s| s.kill_switches.disable_code_interpreter = true);
    let (st, _, json) = upload(&t, &c, chat, "data.xlsx", XLSX, b"PK").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(reason(&json), "CODE_INTERPRETER_UNAVAILABLE");
    // Documents with another purpose still upload.
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert!(rows(&t, chat).await.iter().all(|r| r.content_type != XLSX));
}

#[tokio::test]
async fn image_rejected_when_images_disabled() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.policy.with_snapshot(|s| s.kill_switches.disable_images = true);
    let (st, _, json) = upload(&t, &c, chat, "p.png", "image/png", &png(4, 4)).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["context"]["violations"][0]["subject"], "images");
    assert_eq!(json["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert!(rows(&t, chat).await.is_empty());
}

#[tokio::test]
async fn unsupported_and_inferred_content_types() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "a.exe", "application/x-msdownload", b"MZ").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(reason(&json), "UNSUPPORTED_CONTENT_TYPE");
    let (st, _, json) = upload(&t, &c, chat, "blob.bin", "application/octet-stream", b"??").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(reason(&json), "UNSUPPORTED_CONTENT_TYPE");
    let (st, _, json) = upload(&t, &c, chat, "doc.pdf", "application/octet-stream", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["content_type"], "application/pdf");
    assert!(rows(&t, chat).await.len() == 1, "rejected uploads insert no row");
}

#[tokio::test]
async fn csv_is_stored_as_text_when_allowed() {
    let t = TestApp::with_config(|c| c.rag.allow_csv_upload = true).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "a.csv", "text/csv", b"a,b\n1,2").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["content_type"], "text/plain");
    let file_part = requests(&t, "POST", "/v1/files")[0].multipart.iter().find(|p| p.0 == "file").cloned().unwrap();
    assert_eq!(file_part.2.as_deref(), Some("text/plain"));
    assert!(std::path::Path::new(&file_part.1.unwrap()).extension().is_some_and(|e| e == "txt"));
}

#[tokio::test]
async fn csv_rejected_when_not_allowed() {
    let t = TestApp::with_config(|c| c.rag.allow_csv_upload = false).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "a.csv", "text/csv", b"a,b").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(reason(&json), "UNSUPPORTED_CONTENT_TYPE");
}

#[tokio::test]
async fn filename_defaults_and_truncation() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let body = multipart_body(&[("file", None, Some("text/plain"), b"hello")]);
    let (st, _, json) = upload_raw(&t, &c, chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["filename"], "upload");
    let long = format!("{}.txt", "n".repeat(300));
    let (st, _, json) = upload(&t, &c, chat, &long, "text/plain", b"x").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    let name = json["filename"].as_str().unwrap();
    assert_eq!(name.chars().count(), 255);
    assert!(std::path::Path::new(name).extension().is_some_and(|e| e == "txt"));
}

// ───────────────────────────── upload: request errors ─────────────────────────────

#[tokio::test]
async fn multipart_errors() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let body = multipart_body(&[("file", Some("a.txt"), Some("text/plain"), b"x")]);
    let (st, _, json) = upload_raw(&t, &c, chat, "multipart/form-data", body.clone()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!((field(&json), reason(&json)), ("content_type", "BOUNDARY_REQUIRED"));

    let body = multipart_body(&[("other", None, None, b"x")]);
    let (st, _, json) = upload_raw(&t, &c, chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!((field(&json), reason(&json)), ("file", "MISSING_FILE"));

    let body = multipart_body(&[("file", Some("a.txt"), None, b"x")]);
    let (st, _, json) = upload_raw(&t, &c, chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!((field(&json), reason(&json)), ("content_type", "MISSING_CONTENT_TYPE"));

    let body = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nunterminated");
    let (st, _, json) = upload_raw(&t, &c, chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body.into_bytes()).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!((field(&json), reason(&json)), ("multipart", "MULTIPART_ERROR"));
    assert!(rows(&t, chat).await.is_empty());
}

#[tokio::test]
async fn file_too_large_uses_the_kind_limit() {
    let t = TestApp::with_config(|c| {
        c.rag.uploaded_file_max_size_kb = 1;
        c.rag.uploaded_image_max_size_kb = 2;
    })
    .await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let (st, _, json) = upload(&t, &c, chat, "a.txt", "text/plain", &[b'a'; 1025]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(json["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.out_of_range.v1~");
    assert_eq!((field(&json), reason(&json)), ("content_length", "FILE_TOO_LARGE"));
    let (st, _, json) = upload(&t, &c, chat, "a.txt", "text/plain", &[b'a'; 1024]).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    // Images use the image limit (2 KiB here).
    let (st, _, json) = upload(&t, &c, chat, "p.png", "image/png", &[7_u8; 2049]).await;
    assert_eq!(reason(&json), "FILE_TOO_LARGE", "{st} {json}");
    let (st, _, json) = upload(&t, &c, chat, "p.png", "image/png", &[7_u8; 2048]).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(requests(&t, "POST", "/v1/files").len(), 2);
}

#[tokio::test]
async fn model_file_size_limit_applies() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.policy.with_snapshot(|s| {
        for m in &mut s.model_catalog {
            m.general_config.max_file_size_mb = 1;
        }
    });
    let (st, _, json) = upload(&t, &c, chat, "a.txt", "text/plain", &vec![b'a'; 1024 * 1024 + 1]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(reason(&json), "FILE_TOO_LARGE");
}

#[tokio::test]
async fn per_chat_document_and_storage_limits() {
    let t = TestApp::with_config(|c| {
        c.rag.max_documents_per_chat = 1;
        c.rag.max_total_upload_mb_per_chat = 1;
    })
    .await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    upload_doc(&t, &c, chat).await;
    let (st, _, json) = upload(&t, &c, chat, "b.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{json}");
    assert_eq!(json["context"]["violations"][0]["subject"], "document_limit");
    // Images are not counted as documents but count toward storage.
    let (st, _, json) = upload(&t, &c, chat, "p.png", "image/png", &png(8, 8)).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    let big = vec![0_u8; 1024 * 1024];
    let (st, _, json) = upload(&t, &c, chat, "p.gif", "image/gif", &big).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{json}");
    assert_eq!(json["context"]["violations"][0]["subject"], "storage_limit");
    assert_eq!(rows(&t, chat).await.len(), 2, "rejected uploads insert no row");
}

#[tokio::test]
async fn storage_limit_counts_non_failed_rows() {
    let t = TestApp::with_config(|c| c.rag.max_total_upload_mb_per_chat = 1).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let half = vec![b'a'; 600 * 1024];
    let (st, _, json) = upload(&t, &c, chat, "a.txt", "text/plain", &half).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    let (st, _, json) = upload(&t, &c, chat, "b.txt", "text/plain", &half).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{json}");
    assert_eq!(json["context"]["violations"][0]["subject"], "storage_limit");
    // A failed row does not count.
    t.provider.upload_statuses.lock().unwrap().push_back(500);
    let part = vec![b'b'; 400 * 1024];
    let (st, _, _) = upload(&t, &c, chat, "c.txt", "text/plain", &part).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    let (st, _, json) = upload(&t, &c, chat, "d.txt", "text/plain", &part).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
}

#[tokio::test]
async fn removed_chat_model_is_invalid_model() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, Some(STANDARD)).await;
    t.policy.with_snapshot(|s| s.model_catalog.retain(|m| m.id != STANDARD));
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!((field(&json), reason(&json)), ("model", "INVALID_MODEL"));
    // Even an invalid body is not read.
    let (st, _, json) = upload_raw(&t, &c, chat, "multipart/form-data", Vec::new()).await;
    assert_eq!(reason(&json), "INVALID_MODEL", "{st}");
}

#[tokio::test]
async fn policy_failure_is_internal() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.policy.fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let (st, _, _) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn foreign_unknown_or_deleted_chat_is_404() {
    let t = TestApp::new().await;
    let chat = t.create_chat(&ctx_a1(), None).await;
    let (st, _, json) = upload(&t, &ctx(USER_A2, TENANT_A), chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{json}");
    assert_eq!(json["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    let (st, _, _) = upload(&t, &ctx_a1(), Uuid::new_v4(), "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = t.call(&ctx_a1(), "DELETE", &format!("/mini-chat/v1/chats/{chat}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _, json) = upload(&t, &ctx_a1(), chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{json}");
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, _)| a == "upload_attachment"));
}

#[tokio::test]
async fn concurrency_limit_is_503() {
    let t = TestApp::with_config(|c| c.rag.max_concurrent_uploads = 1).await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let permit = std::sync::Arc::clone(&t.app.upload_permits).try_acquire_owned().unwrap();
    let (st, headers, _) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers.get("retry-after").unwrap(), "5");
    drop(permit);
    upload_doc(&t, &c, chat).await;
}

// ───────────────────────────── upload: provider failures ─────────────────────────────

#[tokio::test]
async fn provider_upload_failure_marks_row_failed() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.provider.upload_statuses.lock().unwrap().push_back(500);
    let (st, headers, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_eq!(headers.get("retry-after").unwrap(), "10");
    assert_eq!(json["detail"], "Service temporarily unavailable");
    let r = &rows(&t, chat).await[0];
    let (st, _, got) = t.call(&c, "GET", &att_uri(chat, r.id), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["status"], "failed");
    assert_eq!(got["error_code"], "upload_failed");
    // 4xx from the provider is a provider failure too.
    t.provider.upload_statuses.lock().unwrap().push_back(400);
    let (st, _, _) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn indexing_failure_within_deadline_is_503() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.provider.vector_file_status.lock().unwrap().extend(["in_progress".to_owned(), "failed".to_owned()]);
    let (st, headers, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_eq!(headers.get("retry-after").unwrap(), "10");
    assert!(!json.to_string().contains("indexing_failed"));
    let r = &rows(&t, chat).await[0];
    let (_, _, got) = t.call(&c, "GET", &att_uri(chat, r.id), None).await;
    assert_eq!(got["status"], "failed");
    assert_eq!(got["error_code"], "indexing_failed");
    // Best-effort provider file delete.
    t.eventually("provider file deleted", || async { file_deletes(&t) == 1 }).await;
    assert!(row(&t, r.id).await.cleanup_status.is_none());
}

#[tokio::test]
async fn indexing_still_running_finishes_in_background() {
    let t = TestApp::new().await;
    indexing::set_timings(&t.app, IndexingTimings::for_tests());
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    {
        let mut q = t.provider.vector_file_status.lock().unwrap();
        q.extend(std::iter::repeat_n("in_progress".to_owned(), 14));
        q.push_back("completed".to_owned());
    }
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "uploaded");
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    let uri = att_uri(chat, id);
    t.eventually("background indexing completes", || async {
        let (_, _, got) = t.call(&c, "GET", &uri, None).await;
        got["status"] == "ready"
    })
    .await;
    assert!(row(&t, id).await.cleanup_status.is_none());
}

#[tokio::test]
async fn background_indexing_failure_hands_file_to_cleanup() {
    let t = TestApp::new().await;
    indexing::set_timings(&t.app, IndexingTimings::for_tests());
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    {
        let mut q = t.provider.vector_file_status.lock().unwrap();
        q.extend(std::iter::repeat_n("in_progress".to_owned(), 14));
        q.push_back("cancelled".to_owned());
    }
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "uploaded");
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    t.eventually("indexing failure + cleanup", || async {
        let r = row(&t, id).await;
        r.status == "failed" && r.cleanup_status.as_deref() == Some("done")
    })
    .await;
    let r = row(&t, id).await;
    assert_eq!(r.error_code.as_deref(), Some("indexing_failed"));
    assert!(r.deleted_at.is_none());
    assert_eq!(file_deletes(&t), 1);
}

#[tokio::test]
async fn background_indexing_times_out() {
    let t = TestApp::new().await;
    let mut timings = IndexingTimings::for_tests();
    timings.background_total = Duration::from_millis(400);
    indexing::set_timings(&t.app, timings);
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.provider.vector_file_status.lock().unwrap().extend(std::iter::repeat_n("in_progress".to_owned(), 500));
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    t.eventually("indexing timeout", || async { row(&t, id).await.status == "failed" }).await;
    assert_eq!(row(&t, id).await.error_code.as_deref(), Some("indexing_failed"));
}

#[tokio::test]
async fn background_task_stops_for_deleted_attachment() {
    let t = TestApp::new().await;
    indexing::set_timings(&t.app, IndexingTimings::for_tests());
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    {
        let mut q = t.provider.vector_file_status.lock().unwrap();
        q.extend(std::iter::repeat_n("in_progress".to_owned(), 30));
        q.push_back("completed".to_owned());
    }
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    let (st, _, _) = t.call(&c, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    tokio::time::sleep(Duration::from_millis(800)).await;
    let r = row(&t, id).await;
    assert_eq!(r.status, "uploaded", "a deleted row never becomes ready");
}

// ───────────────────────────── get ─────────────────────────────

#[tokio::test]
async fn get_attachment_isolation() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let id = upload_doc(&t, &c, chat).await;
    let (st, _, got) = t.call(&c, "GET", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::OK, "{got}");
    assert_eq!(got["id"], id.to_string());
    assert_eq!(got["status"], "ready");
    assert!(got.get("error_code").is_none() && got.get("img_thumbnail").is_none() && got.get("doc_summary").is_none());
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, _)| a == "read_attachment"));

    let not_found_attachment = |json: &Value| json["context"]["resource_type"] == "gts.cf.core.mini_chat.attachment.v1~";
    // Unknown id.
    let (st, _, json) = t.call(&c, "GET", &att_uri(chat, Uuid::new_v4()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(not_found_attachment(&json), "{json}");
    // Attachment of another chat.
    let chat2 = t.create_chat(&c, None).await;
    let (st, _, json) = t.call(&c, "GET", &att_uri(chat2, id), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(not_found_attachment(&json));
    // Uploaded by another user in the caller's chat.
    let other = insert_row(&t, row_template(TENANT_A, chat, USER_A2, "ready", crate::clock::now())).await;
    let (st, _, json) = t.call(&c, "GET", &att_uri(chat, other), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(not_found_attachment(&json));
    // Another user cannot see the chat at all.
    let (st, _, json) = t.call(&ctx(USER_A2, TENANT_A), "GET", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(json["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    // Non-UUID path.
    let (st, _, _) = t.call(&c, "GET", &format!("/mini-chat/v1/chats/{chat}/attachments/nope"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

// ───────────────────────────── delete ─────────────────────────────

#[tokio::test]
async fn delete_is_idempotent_and_cleans_up_once() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let id = upload_doc(&t, &c, chat).await;
    let (st, _, _) = t.call(&c, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(t.authz.calls.lock().unwrap().iter().any(|(a, _)| a == "delete_attachment"));
    let (st, _, _) = t.call(&c, "GET", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _, _) = t.call(&c, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    t.eventually("cleanup done", || async { row(&t, id).await.cleanup_status.as_deref() == Some("done") }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(file_deletes(&t), 1, "exactly one cleanup event");
    let r = row(&t, id).await;
    assert!(r.deleted_at.is_some());
    // The vector store is not touched by attachment deletion.
    assert!(requests(&t, "DELETE", "/vector_stores/").is_empty());
    assert_eq!(vector_store_rows(&t, chat).await.len(), 1);
}

#[tokio::test]
async fn delete_checks_uploader_and_existence() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let other = insert_row(&t, row_template(TENANT_A, chat, USER_A2, "ready", crate::clock::now())).await;
    let (st, _, json) = t.call(&c, "DELETE", &att_uri(chat, other), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(json["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
    let (st, _, _) = t.call(&c, "DELETE", &att_uri(chat, Uuid::new_v4()), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert!(row(&t, other).await.deleted_at.is_none());
}

#[tokio::test]
async fn delete_of_referenced_attachment_is_locked() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    let id = upload_doc(&t, &c, chat).await;
    insert_message_link(&t, TENANT_A, chat, id, false).await;
    let (st, _, json) = t.call(&c, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["context"]["resource_name"], "attachment_locked");
    assert_eq!(json["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.already_exists.v1~");
    assert!(row(&t, id).await.deleted_at.is_none());

    // Only a soft-deleted message references it: deletable.
    let id2 = upload_doc(&t, &c, chat).await;
    insert_message_link(&t, TENANT_A, chat, id2, true).await;
    let (st, _, json) = t.call(&c, "DELETE", &att_uri(chat, id2), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{json}");
}

#[tokio::test]
async fn failed_attachment_without_file_is_cleaned_without_provider_call() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    t.provider.upload_statuses.lock().unwrap().push_back(500);
    let (st, _, _) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    let id = rows(&t, chat).await[0].id;
    let (st, _, _) = t.call(&c, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    t.eventually("cleanup done", || async { row(&t, id).await.cleanup_status.as_deref() == Some("done") }).await;
    assert_eq!(file_deletes(&t), 0);
}

async fn insert_vector_store_row(t: &TestApp, chat: Uuid, provider: &str, vs_id: Option<&str>, age_secs: i64) {
    use sea_orm::ActiveValue::Set;
    let am = crate::infra::db::entities::chat_vector_store::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat),
        vector_store_id: Set(vs_id.map(str::to_owned)),
        provider: Set(provider.to_owned()),
        file_count: Set(0),
        created_at: Set(crate::clock::normalize(crate::clock::now() - time::Duration::seconds(age_secs))),
    };
    let conn = t.app.db.conn().unwrap();
    toolkit_db::secure::secure_insert::<crate::infra::db::entities::chat_vector_store::Entity>(
        am,
        &toolkit_security::AccessScope::for_tenant(TENANT_A),
        &conn,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn vector_store_of_another_backend_is_provider_mismatch() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    insert_vector_store_row(&t, chat, "azure-eu", Some("vs_other"), 0).await;
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["context"]["resource_name"], "provider_mismatch");
    assert!(requests(&t, "POST", "/v1/files").is_empty());
    // Images do not use the vector store.
    let (st, _, json) = upload(&t, &c, chat, "p.png", "image/png", &png(4, 4)).await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
}

#[tokio::test]
async fn stale_placeholder_is_reclaimed() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    insert_vector_store_row(&t, chat, "openai", None, 121).await;
    let (st, _, json) = upload(&t, &c, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(st, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "ready");
    let vs = vector_store_rows(&t, chat).await;
    assert_eq!(vs.len(), 1);
    assert!(vs[0].vector_store_id.is_some());
}

#[tokio::test]
async fn existing_store_is_reused() {
    let t = TestApp::new().await;
    let c = ctx_a1();
    let chat = t.create_chat(&c, None).await;
    insert_vector_store_row(&t, chat, "openai", Some("vs_existing"), 10).await;
    upload_doc(&t, &c, chat).await;
    assert!(requests(&t, "POST", "/v1/vector_stores").iter().all(|r| r.uri.contains("/vs_existing/files")));
}
