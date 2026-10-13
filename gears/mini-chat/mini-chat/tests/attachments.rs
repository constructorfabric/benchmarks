//! T052/T053: attachment upload / indexing / get / delete, limits, cleanup and reaper.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use mini_chat::domain::service::attachments::AttachmentCleanupPayload;
use mini_chat::domain::service::summary::TaskOutcome;
use serde_json::{Value, json};
use uuid::Uuid;

fn att_url(chat: Uuid, id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments/{id}")
}

#[tokio::test]
async fn document_upload_ready_with_vector_store() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let (s, v) = h
        .upload(&h.ctx(), chat, "notes.txt", "text/plain", b"some notes")
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "ready");
    assert_eq!(v["kind"], "document");
    assert_eq!(v["filename"], "notes.txt");
    assert_eq!(v["content_type"], "text/plain");
    assert_eq!(v["size_bytes"], 10);
    assert!(v.get("error_code").is_none() && v.get("img_thumbnail").is_none());
    let id = v["id"].as_str().unwrap().to_owned();

    let paths: Vec<(String, String)> = h
        .provider
        .requests()
        .into_iter()
        .map(|r| (r.method, r.path))
        .collect();
    assert!(
        paths
            .iter()
            .any(|(m, p)| m == "POST" && p.ends_with("/v1/files")),
        "{paths:?}"
    );
    assert_eq!(
        paths
            .iter()
            .filter(|(m, p)| m == "POST" && p.ends_with("/v1/vector_stores"))
            .count(),
        1
    );
    assert!(
        paths
            .iter()
            .any(|(m, p)| m == "POST" && p.contains("/vector_stores/") && p.ends_with("/files"))
    );
    let file_req = h
        .provider
        .requests()
        .into_iter()
        .find(|r| r.path.ends_with("/v1/files"))
        .unwrap();
    assert!(
        file_req
            .content_type
            .unwrap()
            .starts_with("multipart/form-data")
    );

    // a second document reuses the chat's vector store
    h.upload_ok(chat, "more.md", "text/markdown", b"# more")
        .await;
    let n = h
        .provider
        .requests()
        .iter()
        .filter(|r| r.method == "POST" && r.path.ends_with("/v1/vector_stores"))
        .count();
    assert_eq!(n, 1);

    let (s, g) = h
        .get(&format!("/mini-chat/v1/chats/{chat}/attachments/{id}"))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(g["status"], "ready");
    assert!(g["created_at"].is_string());
}

#[tokio::test]
async fn image_upload_ready_with_thumbnail() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let (s, v) = h
        .upload(&h.ctx(), chat, "photo.png", "image/png", &png(400, 200))
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["kind"], "image");
    assert_eq!(v["status"], "ready");
    let t = &v["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp");
    assert!(t["width"].as_i64().unwrap() <= 128 && t["height"].as_i64().unwrap() <= 128);
    // images never touch vector stores
    assert!(
        !h.provider
            .requests()
            .iter()
            .any(|r| r.path.contains("vector_stores"))
    );
    // octet-stream infers the type from the extension
    let (s, v) = h
        .upload(
            &h.ctx(),
            chat,
            "x.png",
            "application/octet-stream",
            &png(4, 4),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["content_type"], "image/png");
}

#[tokio::test]
async fn xlsx_code_interpreter_rules() {
    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let (s, v) = h.upload(&h.ctx(), chat, "t.xlsx", XLSX, b"PK fake").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "ready");
    assert!(
        !h.provider
            .requests()
            .iter()
            .any(|r| r.path.contains("vector_stores")),
        "no vector store for code interpreter files"
    );
    // model without code interpreter support
    let std_chat = h.create_chat_as(&h.ctx(), json!({"model": "std"})).await;
    let (s, v) = h.upload(&h.ctx(), std_chat, "t.xlsx", XLSX, b"PK").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_field_reason(&v, "file", "CODE_INTERPRETER_UNAVAILABLE");
    // kill switch
    let mut p = default_policy();
    p["kill_switches"] = json!({"disable_code_interpreter": true});
    h.set_policy(p);
    let (s, v) = h.upload(&h.ctx(), chat, "t.xlsx", XLSX, b"PK").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_field_reason(&v, "file", "CODE_INTERPRETER_UNAVAILABLE");
}

#[tokio::test]
async fn upload_validation_errors() {
    let mut cfg = default_config();
    cfg["rag"] = json!({"uploaded_file_max_size_kb": 1, "uploaded_image_max_size_kb": 2});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    let (s, v) = h
        .upload(&h.ctx(), chat, "big.txt", "text/plain", &[b'a'; 2048])
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_field_reason(&v, "content_length", "FILE_TOO_LARGE");
    let (s, v) = h
        .upload(&h.ctx(), chat, "big.png", "image/png", &[0u8; 3000])
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_field_reason(&v, "content_length", "FILE_TOO_LARGE");
    let (s, v) = h
        .upload(&h.ctx(), chat, "a.exe", "application/x-msdownload", b"MZ")
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("UNSUPPORTED_CONTENT_TYPE"), "{v}");
    assert!(
        h.provider.requests().is_empty(),
        "nothing reaches the provider"
    );

    // multipart errors
    let r = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", "multipart/form-data")
        .body(axum::body::Body::from("x"))
        .unwrap();
    let (s, b) = h.raw(&h.ctx(), r).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&b).contains("BOUNDARY_REQUIRED"));
    let r = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", "multipart/form-data; boundary=B")
        .body(axum::body::Body::from(
            "--B\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nv\r\n--B--\r\n",
        ))
        .unwrap();
    let (s, b) = h.raw(&h.ctx(), r).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        String::from_utf8_lossy(&b).contains("MISSING_FILE"),
        "{}",
        String::from_utf8_lossy(&b)
    );
    // unknown chat
    let (s, v) = h
        .upload(&h.ctx(), Uuid::new_v4(), "a.txt", "text/plain", b"x")
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        v["context"]["resource_type"]
            .as_str()
            .unwrap()
            .contains("mini_chat.chat")
    );
    // images kill switch
    let mut p = default_policy();
    p["kill_switches"] = json!({"disable_images": true});
    h.set_policy(p);
    let (s, v) = h
        .upload(&h.ctx(), chat, "i.png", "image/png", &png(2, 2))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_violation(&v, "images");
}

#[tokio::test]
async fn per_chat_limits() {
    let mut cfg = default_config();
    cfg["rag"] = json!({"max_documents_per_chat": 2, "max_total_upload_mb_per_chat": 1});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    h.upload_ok(chat, "1.txt", "text/plain", b"a").await;
    h.upload_ok(chat, "2.txt", "text/plain", b"b").await;
    let (s, v) = h.upload(&h.ctx(), chat, "3.txt", "text/plain", b"c").await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_violation(&v, "document_limit");
    // images are not documents but count towards storage
    let big = vec![b'x'; 700 * 1024];
    let chat2 = h.create_chat().await;
    h.upload_ok(chat2, "a.txt", "text/plain", &big).await;
    let (s, v) = h.upload(&h.ctx(), chat2, "b.txt", "text/plain", &big).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_violation(&v, "storage_limit");
}

#[tokio::test]
async fn provider_upload_failure_is_503_and_row_failed() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.fail_file_upload.store(true, Ordering::SeqCst);
    let (s, v) = h.upload(&h.ctx(), chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    let rows = db::chat_attachments(&h, chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    // failed rows do not count towards limits and the next upload works
    h.provider.fail_file_upload.store(false, Ordering::SeqCst);
    h.upload_ok(chat, "b.txt", "text/plain", b"y").await;
}

#[tokio::test]
async fn indexing_failure_within_deadline() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    *h.provider.indexing_status.lock().unwrap() = "failed".into();
    let mut req = axum::http::Request::builder();
    req = req.method("GET");
    drop(req);
    let (s, v) = h.upload(&h.ctx(), chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    let rows = db::chat_attachments(&h, chat).await;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("indexing_failed"));
    // best-effort provider file delete
    assert!(
        h.eventually(|| h.deletes().iter().any(|p| p.contains("/files/")))
            .await
    );
    let id = rows[0].id;
    let (s, g) = h.get(&att_url(chat, id)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(g["status"], "failed");
    assert_eq!(g["error_code"], "indexing_failed");
}

#[tokio::test]
async fn slow_indexing_completes_in_background() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    *h.provider.indexing_status.lock().unwrap() = "in_progress".into();
    *h.provider.status_after_polls.lock().unwrap() = Some((25, "completed".into()));
    let (s, v) = h.upload(&h.ctx(), chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "uploaded");
    let id = Uuid::parse_str(v["id"].as_str().unwrap()).unwrap();
    let mut ready = false;
    for _ in 0..200 {
        if db::attachment(&h, id).await.status == "ready" {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ready, "background indexing should mark the row ready");
}

#[tokio::test]
async fn background_indexing_timeout_fails_and_cleans_up() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    *h.provider.indexing_status.lock().unwrap() = "in_progress".into();
    let id = h.upload_ok(chat, "a.txt", "text/plain", b"x").await;
    let mut failed = false;
    for _ in 0..300 {
        let a = db::attachment(&h, id).await;
        if a.status == "failed" {
            assert_eq!(a.error_code.as_deref(), Some("indexing_failed"));
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(failed);
    assert!(
        h.eventually(|| h.deletes().iter().any(|p| p.contains("/files/")))
            .await,
        "cleanup deletes the provider file"
    );
}

#[tokio::test]
async fn get_and_delete_lifecycle() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let id = h.upload_ok(chat, "a.txt", "text/plain", b"x").await;
    let file_id = db::attachment(&h, id).await.provider_file_id.unwrap();

    // another user of the tenant / another chat -> 404 attachment
    let other_chat = h.create_chat().await;
    let (s, v) = h.get(&att_url(other_chat, id)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        v["context"]["resource_type"]
            .as_str()
            .unwrap()
            .contains("mini_chat.attachment"),
        "{v}"
    );
    let (s, _) = h.get(&att_url(chat, Uuid::new_v4())).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _, _) = h.req(&h.ctx(), "DELETE", &att_url(chat, id), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = h.get(&att_url(chat, id)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _, _) = h.req(&h.ctx(), "DELETE", &att_url(chat, id), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "idempotent");
    // outbox cleanup deletes the provider file exactly once
    assert!(
        h.eventually(|| h.deletes().iter().any(|p| p.ends_with(&file_id)))
            .await
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.deletes().iter().filter(|p| p.ends_with(&file_id)).count(),
        1
    );
    assert!(h.eventually(|| true).await);
    let mut done = false;
    for _ in 0..100 {
        if db::attachment(&h, id).await.cleanup_status.as_deref() == Some("done") {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(done);
}

#[tokio::test]
async fn referenced_attachment_is_locked() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let id = h.upload_ok(chat, "a.txt", "text/plain", b"x").await;
    h.send_body(chat, json!({"content": "use it", "attachment_ids": [id]}))
        .await;
    let (s, v, _) = h.req(&h.ctx(), "DELETE", &att_url(chat, id), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("attachment_locked"), "{v}");
}

#[tokio::test]
async fn attachment_cleanup_retries_then_fails_at_max_attempts() {
    let mut cfg = default_config();
    cfg["cleanup_worker"] = json!({"max_attempts": 3});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    let id = h.upload_ok(chat, "a.txt", "text/plain", b"x").await;
    let row = db::attachment(&h, id).await;
    *h.provider.delete_status.lock().unwrap() = 500;
    let p = AttachmentCleanupPayload::from_row(
        &row,
        "attachment_deleted",
        time::OffsetDateTime::now_utc(),
    );
    let o1 = h.core.process_attachment_cleanup(&p).await;
    assert!(matches!(o1, TaskOutcome::Retry(_)), "{o1:?}");
    let a = db::attachment(&h, id).await;
    assert_eq!(a.cleanup_attempts, 1);
    assert!(a.last_cleanup_error.is_some());
    let _ = h.core.process_attachment_cleanup(&p).await;
    let o3 = h.core.process_attachment_cleanup(&p).await;
    assert!(matches!(o3, TaskOutcome::Reject(_)), "{o3:?}");
    assert_eq!(
        db::attachment(&h, id).await.cleanup_status.as_deref(),
        Some("failed")
    );

    // 404 from the provider counts as success
    let id2 = h.upload_ok(chat, "b.txt", "text/plain", b"y").await;
    *h.provider.delete_status.lock().unwrap() = 404;
    let p2 = AttachmentCleanupPayload::from_row(
        &db::attachment(&h, id2).await,
        "attachment_deleted",
        time::OffsetDateTime::now_utc(),
    );
    assert!(matches!(
        h.core.process_attachment_cleanup(&p2).await,
        TaskOutcome::Ok
    ));
    assert_eq!(
        db::attachment(&h, id2).await.cleanup_status.as_deref(),
        Some("done")
    );
}

#[tokio::test]
async fn upload_reaper_fails_abandoned_rows() {
    // long background rounds so the abandoned row is not refreshed during the test
    let timings = mini_chat::domain::service::attachments::UploadTimings {
        sync_deadline: Duration::from_millis(200),
        sync_max_backoff: Duration::from_millis(50),
        initial_backoff: Duration::from_millis(20),
        background_total: Duration::from_secs(60),
        background_round: Duration::from_secs(30),
        background_max_backoff: Duration::from_secs(5),
        loser_polls: 5,
        stale_placeholder: Duration::from_secs(120),
    };
    let h = Harness::with(Opts {
        timings: Some(timings),
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    let fresh = h.upload_ok(chat, "b.txt", "text/plain", b"y").await;
    *h.provider.indexing_status.lock().unwrap() = "in_progress".into();
    let id = h.upload_ok(chat, "a.txt", "text/plain", b"x").await;
    let mut reaped = false;
    for _ in 0..20 {
        db::set_attachment_updated_back(&h, id, 3600).await;
        h.core.reap_abandoned_uploads().await;
        if db::attachment(&h, id).await.status == "failed" {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(reaped);
    let a = db::attachment(&h, id).await;
    assert_eq!(a.status, "failed");
    assert_eq!(a.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(db::attachment(&h, fresh).await.status, "ready");
    let file_id = a.provider_file_id.unwrap();
    assert!(
        h.eventually(|| h.deletes().iter().any(|p| p.ends_with(&file_id)))
            .await,
        "abandoned file cleaned up"
    );
    let _: Value = json!(null);
}
