//! Attachments: upload/get/delete for documents and images, type/size/limit
//! validation, multipart errors, vector-store indexing (sync, background,
//! failure), storage failures, availability to provider tools, cleanup.
#![allow(clippy::many_single_char_names, clippy::expect_used, clippy::case_sensitive_file_extension_comparisons)]

mod common;

use std::io::Cursor;
use std::time::Duration;

use common::*;
use http::StatusCode;
use mini_chat::domain::services::Timings;
use serde_json::json;
use uuid::Uuid;

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 255) as u8, (y % 255) as u8, 128]));
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("png");
    out.into_inner()
}

fn att_uri(chat: Uuid, id: &str) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments/{id}")
}

#[tokio::test(flavor = "multi_thread")]
async fn document_upload_get_delete_lifecycle() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let r = h.upload(&a, chat, "report.pdf", "application/pdf", b"%PDF-1.4 content").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let id = r.body["id"].as_str().unwrap().to_owned();
    assert_eq!(r.body["filename"], "report.pdf");
    assert_eq!(r.body["content_type"], "application/pdf");
    assert_eq!(r.body["size_bytes"], 16);
    assert_eq!(r.body["status"], "ready");
    assert_eq!(r.body["kind"], "document");
    for absent in ["error_code", "doc_summary", "img_thumbnail", "summary_updated_at"] {
        assert!(r.body.get(absent).is_none(), "{absent} omitted: {}", r.text);
    }
    assert!(!r.text.contains("file-"), "no provider ids exposed");

    // provider calls: Files upload, vector store create, add file with attachment attribute
    let files = h.gw.calls("/v1/files");
    let upload = files.iter().find(|c| c.method == "POST").unwrap();
    let body = String::from_utf8_lossy(&upload.body);
    assert!(body.contains("name=\"purpose\"") && body.contains("assistants"));
    assert!(body.contains(&format!("filename=\"{chat}_{id}.pdf\"")), "{body}");
    assert!(upload.headers["content-type"].starts_with("multipart/form-data"));
    let vs_create = h.gw.calls("/v1/vector_stores");
    assert!(vs_create.iter().any(|c| c.method == "POST" && c.uri.ends_with("/vector_stores")));
    let add = vs_create.iter().find(|c| c.uri.ends_with("/files") && c.method == "POST").unwrap();
    assert_eq!(add.json()["attributes"]["attachment_id"], id.as_str());

    let row = h
        .rows(&format!(
            "SELECT provider_file_id, status, attachment_kind, CAST(for_file_search AS TEXT), storage_backend \
             FROM attachments WHERE id = {}",
            blob(Uuid::parse_str(&id).unwrap())
        ))
        .await;
    assert!(row[0][0].as_deref().unwrap().starts_with("file-"));
    assert_eq!(row[0][1].as_deref(), Some("ready"));
    assert_eq!(row[0][2].as_deref(), Some("document"));
    assert_eq!(row[0][3].as_deref(), Some("1"));
    assert_eq!(
        h.scalar(&format!("SELECT COUNT(*) FROM chat_vector_stores WHERE chat_id = {} AND vector_store_id IS NOT NULL", blob(chat)))
            .await,
        1
    );

    let g = h.get(&att_uri(chat, &id), &a).await;
    assert_eq!(g.status, StatusCode::OK);
    assert_eq!(g.body["id"], id.as_str());
    assert_eq!(g.body["status"], "ready");

    // delete → 204, idempotent, then 404 on GET; provider file deleted via the outbox
    let d = h.req("DELETE", &att_uri(chat, &id), &a, None).await;
    assert_eq!(d.status, StatusCode::NO_CONTENT);
    let d = h.req("DELETE", &att_uri(chat, &id), &a, None).await;
    assert_eq!(d.status, StatusCode::NO_CONTENT, "repeated delete is idempotent");
    let g = h.get(&att_uri(chat, &id), &a).await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
    assert_eq!(g.body["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
    h.eventually("provider file deleted", || async {
        h.gw.calls("/v1/files/").iter().any(|c| c.method == "DELETE").then_some(())
    })
    .await;
    h.drain_outbox().await;
    let st = h
        .rows(&format!("SELECT cleanup_status FROM attachments WHERE id = {}", blob(Uuid::parse_str(&id).unwrap())))
        .await;
    assert_eq!(st[0][0].as_deref(), Some("done"));
    let r = h.get(&att_uri(chat, &Uuid::new_v4().to_string()), &a).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn image_upload_has_thumbnail_and_no_vector_store() {
    use base64::Engine;
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let r = h.upload(&a, chat, "pic.png", "image/png", &png(400, 300)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["kind"], "image");
    assert_eq!(r.body["status"], "ready");
    let t = &r.body["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp");
    assert!(t["width"].as_i64().unwrap() <= 128 && t["height"].as_i64().unwrap() <= 128);
    assert_eq!(t["width"], 128);
    assert_eq!(t["height"], 96);
    let raw = base64::engine::general_purpose::STANDARD
        .decode(t["data_base64"].as_str().unwrap())
        .unwrap();
    assert!(raw.len() <= 131_072);
    assert_eq!(&raw[0..4], b"RIFF");
    assert!(h.gw.calls("/vector_stores").is_empty(), "images are not indexed");

    // images go to the provider as input_image
    let id = r.body["id"].as_str().unwrap();
    let s = h.send_body(&a, chat, json!({"content": "what is this?", "attachment_ids": [id]})).await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    let call = h.gw.chat_calls().pop().unwrap().json();
    let current = call["input"].as_array().unwrap().last().unwrap().clone();
    let parts = current["content"].as_array().unwrap();
    assert!(parts.iter().any(|p| p["type"] == "input_image" && p["file_id"].as_str().unwrap().starts_with("file-")));
    // thumbnail in message attachments
    let msgs = h.messages(&a, chat).await;
    assert_eq!(msgs[0]["attachments"][0]["kind"], "image");
    assert!(msgs[0]["attachments"][0]["img_thumbnail"]["data_base64"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_type_validation_and_inference() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let r = h.upload(&a, chat, "tool.exe", "application/x-msdownload", b"MZ").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "UNSUPPORTED_CONTENT_TYPE");
    let r = h.upload(&a, chat, "blob.bin", "application/octet-stream", b"??").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "unknown extension stays octet-stream");
    // octet-stream with a known extension is inferred
    let r = h.upload(&a, chat, "doc.pdf", "application/octet-stream", b"%PDF").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["content_type"], "application/pdf");
    // missing filename defaults to "upload"; long names are truncated keeping the extension
    let long = format!("{}.txt", "n".repeat(300));
    let r = h.upload(&a, chat, &long, "text/plain", b"hello").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let name = r.body["filename"].as_str().unwrap();
    assert_eq!(name.chars().count(), 255);
    assert!(name.ends_with(".txt"));
    assert_eq!(
        h.scalar(&format!("SELECT COUNT(*) FROM attachments WHERE chat_id = {}", blob(chat))).await,
        2,
        "rejected types create no rows"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_errors() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let uri = format!("/mini-chat/v1/chats/{chat}/attachments");
    let r = h.raw_body("POST", &uri, &a, Some("multipart/form-data"), "x").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "BOUNDARY_REQUIRED");
    assert_eq!(r.body["context"]["field_violations"][0]["field"], "content_type");

    let body = "--B\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nv\r\n--B--\r\n";
    let r = h.raw_body("POST", &uri, &a, Some("multipart/form-data; boundary=B"), body).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "MISSING_FILE");
    assert_eq!(r.body["context"]["field_violations"][0]["field"], "file");

    let r = h.multipart(&uri, &a, "a.txt", None, b"data").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "MISSING_CONTENT_TYPE");

    let r = h
        .raw_body("POST", &uri, &a, Some("multipart/form-data; boundary=B"), "garbage without boundary")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let reason = r.body["context"]["field_violations"][0]["reason"].as_str().unwrap();
    assert!(reason == "MULTIPART_ERROR" || reason == "MISSING_FILE", "{reason}");
}

#[tokio::test(flavor = "multi_thread")]
async fn size_and_per_chat_limits() {
    let h = Harness::with(Options {
        config: json!({"rag": {"uploaded_file_max_size_kb": 2048, "uploaded_image_max_size_kb": 1,
                               "max_documents_per_chat": 2, "max_total_upload_mb_per_chat": 1}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let r = h.upload(&a, chat, "big.png", "image/png", &vec![0u8; 2048]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["title"], "Out of Range");
    assert_eq!(r.body["context"]["field_violations"][0]["field"], "content_length");
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "FILE_TOO_LARGE");

    let r = h.upload(&a, chat, "a.txt", "text/plain", &vec![b'a'; 700 * 1024]).await;
    assert_eq!(r.status, StatusCode::CREATED);
    // storage limit (1 MB per chat, images included)
    let r = h.upload(&a, chat, "b.txt", "text/plain", &vec![b'b'; 400 * 1024]).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{}", r.text);
    assert_eq!(r.body["context"]["violations"][0]["subject"], "storage_limit");
    // document count limit
    let r = h.upload(&a, chat, "c.txt", "text/plain", b"c").await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = h.upload(&a, chat, "d.txt", "text/plain", b"d").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.body["context"]["violations"][0]["subject"], "document_limit");
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_failures_are_503_and_rows_failed() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.files_fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let r = h.upload(&a, chat, "a.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text);
    assert_eq!(r.headers["retry-after"], "10");
    assert!(!r.text.contains("files api down"));
    let rows = h
        .rows(&format!("SELECT status, error_code FROM attachments WHERE chat_id = {}", blob(chat)))
        .await;
    assert_eq!(rows[0][0].as_deref(), Some("failed"));
    assert_eq!(rows[0][1].as_deref(), Some("upload_failed"));
    h.gw.files_fail.store(false, std::sync::atomic::Ordering::SeqCst);

    // indexing failure within the request deadline
    *h.gw.vs_status.lock() = "failed".to_owned();
    let r = h.upload(&a, chat, "b.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    let rows = h
        .rows(&format!(
            "SELECT status, error_code FROM attachments WHERE chat_id = {} AND filename = 'b.pdf'",
            blob(chat)
        ))
        .await;
    assert_eq!(rows[0][0].as_deref(), Some("failed"));
    assert_eq!(rows[0][1].as_deref(), Some("indexing_failed"));
    // the failed row stays visible with its error code
    let id: String = h
        .rows("SELECT lower(hex(id)) FROM attachments WHERE filename = 'b.pdf'")
        .await[0][0]
        .clone()
        .unwrap();
    let id = Uuid::parse_str(&id).unwrap().to_string();
    let g = h.get(&att_uri(chat, &id), &a).await;
    assert_eq!(g.body["status"], "failed");
    assert_eq!(g.body["error_code"], "indexing_failed");
    h.eventually("provider file of the failed indexing deleted", || async {
        h.gw.calls("/v1/files/").iter().any(|c| c.method == "DELETE").then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_indexing_returns_uploaded_then_becomes_ready() {
    let h = Harness::with(Options {
        timings: Some(Timings {
            sync_index_deadline: Duration::from_millis(400),
            background_round: Duration::from_millis(300),
            background_index_limit: Duration::from_secs(20),
        }),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    *h.gw.vs_status.lock() = "in_progress".to_owned();
    let r = h.upload(&a, chat, "slow.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    assert_eq!(r.body["status"], "uploaded");
    let id = r.body["id"].as_str().unwrap().to_owned();
    // not usable until ready
    let s = h.send_body(&a, chat, json!({"content": "q", "attachment_ids": [id]})).await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.problem["context"]["field_violations"][0]["reason"], "invalid_attachment");
    *h.gw.vs_status.lock() = "completed".to_owned();
    h.eventually("attachment ready", || async {
        let g = h.get(&att_uri(chat, &id), &a).await;
        (g.body["status"] == "ready").then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn background_indexing_failure_marks_failed_and_cleans_up() {
    let h = Harness::with(Options {
        timings: Some(Timings {
            sync_index_deadline: Duration::from_millis(300),
            background_round: Duration::from_millis(200),
            background_index_limit: Duration::from_millis(900),
        }),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    *h.gw.vs_status.lock() = "in_progress".to_owned();
    let r = h.upload(&a, chat, "slow.pdf", "application/pdf", b"%PDF").await;
    assert_eq!(r.body["status"], "uploaded");
    let id = r.body["id"].as_str().unwrap().to_owned();
    h.eventually("indexing timed out", || async {
        let g = h.get(&att_uri(chat, &id), &a).await;
        (g.body["status"] == "failed").then_some(())
    })
    .await;
    let g = h.get(&att_uri(chat, &id), &a).await;
    assert_eq!(g.body["error_code"], "indexing_failed");
    h.eventually("provider file deleted through the outbox", || async {
        h.gw.calls("/v1/files/").iter().any(|c| c.method == "DELETE").then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_concurrency_limit_is_503() {
    let h = std::sync::Arc::new(
        Harness::with(Options {
            config: json!({"rag": {"max_concurrent_uploads": 1}}),
            timings: Some(Timings {
                sync_index_deadline: Duration::from_secs(2),
                background_round: Duration::from_millis(200),
                background_index_limit: Duration::from_secs(5),
            }),
            ..Options::default()
        })
        .await,
    );
    let a = user_a();
    let chat = h.create_chat(&a).await;
    *h.gw.vs_status.lock() = "in_progress".to_owned();
    let h2 = std::sync::Arc::clone(&h);
    let a2 = a.clone();
    let slow = tokio::spawn(async move { h2.upload(&a2, chat, "slow.pdf", "application/pdf", b"%PDF").await });
    h.eventually("first upload holds the slot", || async {
        (!h.gw.calls("/vector_stores/").is_empty()).then_some(())
    })
    .await;
    let r = h.upload(&a, chat, "second.txt", "text/plain", b"x").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text);
    assert_eq!(r.headers["retry-after"], "5");
    let first = slow.await.unwrap();
    assert_eq!(first.status, StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_switch_code_interpreter_and_model_checks() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let r = h.upload(&a, chat, "sheet.xlsx", xlsx, b"PK\x03\x04").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
    let row = h
        .rows(&format!(
            "SELECT CAST(for_code_interpreter AS TEXT), CAST(for_file_search AS TEXT) FROM attachments WHERE chat_id = {}",
            blob(chat)
        ))
        .await;
    assert_eq!(row[0][0].as_deref(), Some("1"));
    assert_eq!(row[0][1].as_deref(), Some("0"));

    h.policy.update(|s| s.kill_switches.disable_code_interpreter = true);
    let r = h.upload(&a, chat, "sheet2.xlsx", xlsx, b"PK\x03\x04").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["field"], "file");
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");

    h.policy.update(|s| s.kill_switches.disable_images = true);
    let r = h.upload(&a, chat, "p.png", "image/png", &png(10, 10)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["violations"][0]["subject"], "images");
    assert_eq!(r.body["context"]["violations"][0]["type"], "FEATURE_DISABLED");

    // chat model removed from the catalog → INVALID_MODEL before reading the body
    h.policy.update(|s| s.model_catalog.retain(|m| m.id != "gpt-premium"));
    let r = h.upload(&a, chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "INVALID_MODEL");
}

#[tokio::test(flavor = "multi_thread")]
async fn referenced_attachment_cannot_be_deleted_and_foreign_access_is_404() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let r = h.upload(&a, chat, "a.txt", "text/plain", b"x").await;
    let id = r.body["id"].as_str().unwrap().to_owned();
    let s = h.send_body(&a, chat, json!({"content": "q", "attachment_ids": [id]})).await;
    assert_eq!(s.names().last(), Some(&"done"));
    let d = h.req("DELETE", &att_uri(chat, &id), &a, None).await;
    assert_eq!(d.status, StatusCode::CONFLICT);
    assert_eq!(d.body["title"], "Already Exists");
    assert_eq!(d.body["context"]["resource_name"], "attachment_locked");
    // attachment from another chat of the same user is invalid in a message
    let other = h.create_chat(&a).await;
    let s = h.send_body(&a, other, json!({"content": "q", "attachment_ids": [id]})).await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.problem["context"]["field_violations"][0]["reason"], "invalid_attachment");
    let g = h.get(&att_uri(other, &id), &a).await;
    assert_eq!(g.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn vision_and_image_count_guards() {
    let h = Harness::with(Options {
        config: json!({"rag": {"max_images_per_message": 1}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let i1 = h.upload(&a, chat, "1.png", "image/png", &png(8, 8)).await.body["id"].clone();
    let i2 = h.upload(&a, chat, "2.png", "image/png", &png(8, 8)).await.body["id"].clone();
    let s = h.send_body(&a, chat, json!({"content": "q", "attachment_ids": [i1, i2]})).await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.problem["context"]["field_violations"][0]["field"], "image_count");
    assert_eq!(s.problem["context"]["field_violations"][0]["reason"], "TOO_MANY_IMAGES");
    // downgrade to a model without VISION_INPUT rejects the image turn
    h.policy.set_limits((100_000_000, 1_000_000_000), (1, 1));
    let s = h.send_body(&a, chat, json!({"content": "q", "attachment_ids": [i1]})).await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST, "{}", s.raw);
    assert_eq!(s.problem["context"]["field_violations"][0]["reason"], "VISION_NOT_SUPPORTED");
    assert!(h.gw.chat_calls().is_empty(), "no provider call");
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_mismatch_is_409() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let r = h.upload(&a, chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(r.status, StatusCode::CREATED);
    h.exec(&format!("UPDATE chat_vector_stores SET provider = 'azure-other' WHERE chat_id = {}", blob(chat)))
        .await;
    let r = h.upload(&a, chat, "b.txt", "text/plain", b"y").await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.text);
    assert_eq!(r.body["context"]["resource_name"], "provider_mismatch");
}
