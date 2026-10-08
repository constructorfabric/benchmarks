//! Attachments: upload / get / delete, indexing lifecycle, tools, cleanup and recovery.

use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use crate::common::*;
use crate::turns::tiny_png;

fn id_of(r: &Resp) -> Uuid {
    Uuid::parse_str(r.json()["id"].as_str().unwrap_or_else(|| panic!("no id: {}", r.text()))).unwrap()
}

/// Upload / get / delete lifecycle with size, type and per-chat limits.
#[tokio::test]
async fn upload_get_delete_lifecycle_and_limits() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    // Document.
    let r = h.upload(U1, chat, "report.pdf", "application/pdf", b"%PDF-1.4 test").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let doc = r.json();
    assert_eq!(doc["status"], "ready");
    assert_eq!(doc["kind"], "document");
    assert_eq!(doc["filename"], "report.pdf");
    assert_eq!(doc["content_type"], "application/pdf");
    assert_eq!(doc["size_bytes"], 13);
    assert!(doc.get("img_thumbnail").is_none());
    assert!(!r.text().contains("file-") && !r.text().contains("vs_"), "no provider ids");
    let doc_id = id_of(&r);
    // octet-stream inferred from extension.
    let r = h.upload(U1, chat, "notes.md", "application/octet-stream", b"# notes").await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["content_type"], "text/markdown");
    // Image with thumbnail.
    let r = h.upload(U1, chat, "pic.png", "image/png", &tiny_png()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let img = r.json();
    assert_eq!(img["kind"], "image");
    assert_eq!(img["status"], "ready");
    assert_eq!(img["img_thumbnail"]["content_type"], "image/webp");
    assert!(img["img_thumbnail"]["width"].as_i64().unwrap() > 0);
    assert!(!img["img_thumbnail"]["data_base64"].as_str().unwrap().is_empty());
    let img_id = id_of(&r);
    // Get.
    let g = h.call(U1, "GET", &format!("/chats/{chat}/attachments/{doc_id}"), None).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["id"], doc_id.to_string());
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}/attachments/{}", Uuid::new_v4()), None).await.status, 404);
    // Unsupported types.
    for (name, ct) in [("a.exe", "application/x-msdownload"), ("a.bin", "application/octet-stream"), ("a.mp4", "video/mp4")] {
        let r = h.upload(U1, chat, name, ct, b"xx").await;
        assert_eq!(r.status, 400, "{ct}");
        assert_eq!(r.reason(), "UNSUPPORTED_CONTENT_TYPE");
    }
    // Multipart errors.
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", "multipart/form-data")
        .body(axum::body::Body::from("x"))
        .unwrap();
    let r = h.send(req).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "BOUNDARY_REQUIRED");
    // Delete is idempotent, provider file cleaned up via outbox.
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/attachments/{doc_id}"), None).await.status, 204);
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/attachments/{doc_id}"), None).await.status, 204);
    assert_eq!(h.call(U1, "GET", &format!("/chats/{chat}/attachments/{doc_id}"), None).await.status, 404);
    assert!(h.wait_until(|| h.provider.calls("DELETE", "/files/") >= 1).await, "provider file deleted");
    // Referenced attachments are locked.
    h.send_message(U1, chat, json!({"content": "see", "attachment_ids": [img_id]})).await;
    let r = h.call(U1, "DELETE", &format!("/chats/{chat}/attachments/{img_id}"), None).await;
    assert_eq!(r.status, 409);
    assert_eq!(r.reason(), "attachment_locked");
    // Another user cannot see the attachment.
    assert_eq!(h.call(U2, "GET", &format!("/chats/{chat}/attachments/{img_id}"), None).await.status, 404);

    // Size and per-chat limits.
    let h = Harness::with(Options {
        config: json!({"rag": {"uploaded_file_max_size_kb": 2, "uploaded_image_max_size_kb": 1, "max_documents_per_chat": 2,
            "max_images_per_message": 1}}),
        ..Options::default()
    })
    .await;
    let chat = h.create_chat(U1, None).await;
    let r = h.upload(U1, chat, "big.txt", "text/plain", &vec![b'a'; 3000]).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "FILE_TOO_LARGE");
    assert_eq!(r.json()["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.out_of_range.v1~");
    let big_png = {
        let img = image::RgbaImage::from_fn(64, 64, |x, y| image::Rgba([(x * 7) as u8, (y * 13) as u8, (x ^ y) as u8, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    };
    assert!(big_png.len() > 1024);
    let r = h.upload(U1, chat, "big.png", "image/png", &big_png).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "FILE_TOO_LARGE");
    for i in 0..2 {
        assert_eq!(h.upload(U1, chat, &format!("d{i}.txt"), "text/plain", b"doc").await.status, 201);
    }
    let r = h.upload(U1, chat, "d3.txt", "text/plain", b"doc").await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "document_limit");
    // Images per message.
    let a = id_of(&h.upload(U1, chat, "a.png", "image/png", &tiny_png()).await);
    let b = id_of(&h.upload(U1, chat, "b.png", "image/png", &tiny_png()).await);
    let r = h.send_message(U1, chat, json!({"content": "x", "attachment_ids": [a, b]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "TOO_MANY_IMAGES");
    // Vision not supported.
    let nv = h.create_chat(U1, Some("novision")).await;
    let img = id_of(&h.upload(U1, nv, "c.png", "image/png", &tiny_png()).await);
    let r = h.send_message(U1, nv, json!({"content": "x", "attachment_ids": [img]})).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.reason(), "VISION_NOT_SUPPORTED");
    assert!(h.provider.chat_requests().is_empty());

    // Storage limit.
    let h = Harness::with(Options { config: json!({"rag": {"max_total_upload_mb_per_chat": 1}}), ..Options::default() }).await;
    let chat = h.create_chat(U1, None).await;
    assert_eq!(h.upload(U1, chat, "a.txt", "text/plain", &vec![b'a'; 700 * 1024]).await.status, 201);
    let r = h.upload(U1, chat, "b.txt", "text/plain", &vec![b'b'; 400 * 1024]).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "storage_limit");

    // Disabled images.
    let h = Harness::with(Options { kill_switches: json!({"disable_images": true}), ..Options::default() }).await;
    let chat = h.create_chat(U1, None).await;
    let r = h.upload(U1, chat, "p.png", "image/png", &tiny_png()).await;
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["context"]["violations"][0]["type"], "FEATURE_DISABLED");
}

/// Asynchronous indexing lifecycle: failure, deadline, background completion and timeout.
#[tokio::test]
async fn indexing_lifecycle_failure_and_timeout() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    // Provider upload failure -> 503, row failed.
    *h.provider.upload_status.lock().unwrap() = 500;
    let r = h.upload(U1, chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(r.status, 503);
    assert_eq!(r.headers["retry-after"], "10");
    *h.provider.upload_status.lock().unwrap() = 200;

    // Indexing fails within the deadline -> 503, row failed with indexing_failed.
    h.provider.index_statuses.lock().unwrap().extend(["in_progress", "failed"]);
    let r = h.upload(U1, chat, "b.txt", "text/plain", b"x").await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert_eq!(r.headers["retry-after"], "10");
    let failed: Vec<_> = {
        let conn = h.app.db.conn().unwrap();
        use mini_chat::infra::db::entity::attachments;
        use sea_orm::{ColumnTrait, Condition, EntityTrait};
        use toolkit_db::secure::SecureEntityExt;
        attachments::Entity::find()
            .secure()
            .scope_with(&toolkit_security::AccessScope::allow_all())
            .filter(Condition::all().add(attachments::Column::ChatId.eq(chat)))
            .all(&conn)
            .await
            .unwrap()
    };
    assert!(failed.iter().any(|a| a.status == "failed" && a.error_code.as_deref() == Some("upload_failed")));
    let idx = failed.iter().find(|a| a.filename == "b.txt").unwrap();
    assert_eq!(idx.status, "failed");
    assert_eq!(idx.error_code.as_deref(), Some("indexing_failed"));
    let g = h.call(U1, "GET", &format!("/chats/{chat}/attachments/{}", idx.id), None).await.json();
    assert_eq!(g["status"], "failed");
    assert_eq!(g["error_code"], "indexing_failed");

    // Still in progress at the deadline -> 201 uploaded, then ready in the background.
    h.provider.index_statuses.lock().unwrap().extend(["in_progress"; 4]);
    let r = h.upload(U1, chat, "c.txt", "text/plain", b"x").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["status"], "uploaded");
    let c = id_of(&r);
    // Not ready yet: cannot be attached.
    let s = h.send_message(U1, chat, json!({"content": "x", "attachment_ids": [c]})).await;
    if s.status == 400 {
        assert_eq!(s.reason(), "invalid_attachment");
    }
    let mut ready = false;
    for _ in 0..200 {
        if h.attachment(c).await.status == "ready" {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready, "background indexing completes");

    // Background indexing fails -> failed + provider file cleanup.
    let deletes = h.provider.calls("DELETE", "/files/");
    h.provider.index_statuses.lock().unwrap().extend(["in_progress", "in_progress", "in_progress", "in_progress", "failed"]);
    let d = id_of(&h.upload(U1, chat, "d.txt", "text/plain", b"x").await);
    let mut failed = false;
    for _ in 0..200 {
        let a = h.attachment(d).await;
        if a.status == "failed" {
            assert_eq!(a.error_code.as_deref(), Some("indexing_failed"));
            failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(failed);
    assert!(h.wait_until(|| h.provider.calls("DELETE", "/files/") > deletes).await);

    // Background indexing timeout (limit 5s in tests) -> failed.
    h.provider.index_statuses.lock().unwrap().extend(["in_progress"; 400]);
    let e = id_of(&h.upload(U1, chat, "e.txt", "text/plain", b"x").await);
    let mut timed_out = false;
    for _ in 0..300 {
        if h.attachment(e).await.status == "failed" {
            timed_out = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(timed_out, "background indexing has a time limit");
    h.provider.index_statuses.lock().unwrap().clear();
}

/// Attachments are made available to the relevant provider tools.
#[tokio::test]
async fn attachments_available_to_provider_tools() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    // No documents: no file_search tool.
    h.send_message(U1, chat, json!({"content": "plain"})).await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req["tools"].as_array().is_none_or(|t| t.iter().all(|x| x["type"] != "file_search")));
    assert!(!req["instructions"].as_str().unwrap_or_default().contains("file_search"));

    let doc = id_of(&h.upload(U1, chat, "q3.pdf", "application/pdf", b"%PDF").await);
    let stores = h.vector_stores(chat).await;
    assert_eq!(stores.len(), 1, "one vector store per chat");
    let vs = stores[0].vector_store_id.clone().unwrap();
    let img = id_of(&h.upload(U1, chat, "p.png", "image/png", &tiny_png()).await);
    let xlsx = h
        .upload(U1, chat, "t.xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", b"PK\x03\x04")
        .await;
    assert_eq!(xlsx.status, 201, "{}", xlsx.text());
    assert_eq!(h.vector_stores(chat).await.len(), 1, "store reused");

    let file_id = h.attachment(doc).await.provider_file_id.unwrap();
    h.provider.push(Script::Sse(vec![
        ("response.file_search_call.searching".into(), json!({})),
        ("response.file_search_call.completed".into(), json!({"results": [{}, {}]})),
        ("response.output_text.delta".into(), json!({"delta": "Revenue grew."})),
        ("response.output_text.annotation.added".into(), json!({"annotation": {"type": "file_citation", "file_id": file_id, "index": 0}})),
        ("response.output_text.annotation.added".into(), json!({"annotation": {"type": "file_citation", "file_id": "file-unknownxxxxxxxx", "index": 0}})),
        ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 10, "output_tokens": 3}}})),
    ]));
    let r = h.send_message(U1, chat, json!({"content": "summarize", "attachment_ids": [img]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let req = h.provider.chat_requests().pop().unwrap();
    let tools = req["tools"].as_array().unwrap();
    let fs = tools.iter().find(|t| t["type"] == "file_search").expect("file_search tool");
    assert_eq!(fs["vector_store_ids"], json!([vs]));
    assert_eq!(fs["max_num_results"], 5);
    let ci = tools.iter().find(|t| t["type"] == "code_interpreter").expect("code_interpreter tool");
    assert_eq!(ci["container"]["file_ids"].as_array().unwrap().len(), 1);
    assert!(req["instructions"].as_str().unwrap().contains("file_search"), "file search guard");
    let last = req["input"].as_array().unwrap().last().unwrap();
    let img_file = h.attachment(img).await.provider_file_id.unwrap();
    assert!(last["content"].as_array().unwrap().iter().any(|p| p["type"] == "input_image" && p["file_id"] == img_file.as_str()));
    // File citations map to attachment ids, unknown files are dropped.
    let cit = r.event("citations").unwrap();
    let items = cit["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], "file");
    assert_eq!(items[0]["attachment_id"], doc.to_string());
    assert_eq!(items[0]["title"], "q3.pdf");
    assert!(!r.text().contains(&file_id));
    let tool = r.events().into_iter().find(|(n, d)| n == "tool" && d["phase"] == "done").unwrap().1;
    assert_eq!(tool["name"], "file_search");
    assert_eq!(tool["details"]["files_searched"], 2);
    // The file search call is counted on the turn and the usage event; the
    // reserved quota_usage counters stay 0 (ADR-0007/0008).
    let rid = Uuid::parse_str(r.event("stream_started").unwrap()["request_id"].as_str().unwrap()).unwrap();
    assert_eq!(h.turn(chat, rid).await.file_search_completed_count, 1);
    let published = h.wait_published(2).await;
    assert_eq!(published.iter().find(|p| p.request_id == rid).unwrap().file_search_calls, 1);
    let q = h.quota(USER_1, "total", "daily").await.unwrap();
    assert_eq!(q.file_search_calls, 0);
    assert_eq!(q.image_inputs, 0);
    assert_eq!(q.image_upload_bytes, 0);

    // Kill switch disables file search.
    let h2 = Harness::with(Options { kill_switches: json!({"disable_file_search": true}), ..Options::default() }).await;
    let chat2 = h2.create_chat(U1, None).await;
    h2.upload(U1, chat2, "q.pdf", "application/pdf", b"%PDF").await;
    h2.send_message(U1, chat2, json!({"content": "x"})).await;
    let req = h2.provider.chat_requests().pop().unwrap();
    assert!(req["tools"].as_array().is_none_or(|t| t.iter().all(|x| x["type"] != "file_search")));
}

/// Cleanup and abandoned-upload recovery behave correctly under failure.
#[tokio::test]
async fn cleanup_and_abandoned_upload_recovery() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    // Cleanup retried after a provider failure.
    *h.provider.delete_status.lock().unwrap() = 500;
    let a = id_of(&h.upload(U1, chat, "a.txt", "text/plain", b"x").await);
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/attachments/{a}"), None).await.status, 204);
    assert!(h.wait_until(|| h.provider.calls("DELETE", "/files/") >= 1).await);
    let row = h.attachment(a).await;
    assert!(row.deleted_at.is_some());
    *h.provider.delete_status.lock().unwrap() = 200;
    let mut done = false;
    for _ in 0..300 {
        let row = h.attachment(a).await;
        if row.cleanup_status.as_deref() == Some("done") {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "cleanup completes after retry: {:?}", h.attachment(a).await.cleanup_status);
    assert!(h.provider.calls("DELETE", "/files/") >= 2, "retried");
    // Provider 404 on delete counts as done.
    *h.provider.delete_status.lock().unwrap() = 404;
    let b = id_of(&h.upload(U1, chat, "b.txt", "text/plain", b"x").await);
    h.call(U1, "DELETE", &format!("/chats/{chat}/attachments/{b}"), None).await;
    let mut done = false;
    for _ in 0..200 {
        if h.attachment(b).await.cleanup_status.as_deref() == Some("done") {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done);
    *h.provider.delete_status.lock().unwrap() = 200;

    // Abandoned upload: stale pending/uploaded rows are reaped.
    let c = id_of(&h.upload(U1, chat, "c.txt", "text/plain", b"x").await);
    h.set_attachment_status(c, "uploaded").await;
    let fresh = id_of(&h.upload(U1, chat, "f.txt", "text/plain", b"x").await);
    h.set_attachment_status(fresh, "pending").await;
    assert_eq!(h.app.reaper_scan().await.unwrap(), 0, "fresh rows are left alone");
    h.age_attachment(c, 600).await;
    let deletes = h.provider.calls("DELETE", "/files/");
    assert_eq!(h.app.reaper_scan().await.unwrap(), 1);
    let row = h.attachment(c).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert!(row.deleted_at.is_none(), "reaped rows stay visible");
    assert_eq!(h.attachment(fresh).await.status, "pending");
    assert!(h.wait_until(|| h.provider.calls("DELETE", "/files/") > deletes).await, "provider file deleted");
    let g = h.call(U1, "GET", &format!("/chats/{chat}/attachments/{c}"), None).await.json();
    assert_eq!(g["error_code"], "upload_abandoned");
    // Rows owned by chat cleanup are skipped by the reaper.
    let d = id_of(&h.upload(U1, chat, "d.txt", "text/plain", b"x").await);
    h.set_attachment_status(d, "uploaded").await;
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}"), None).await.status, 204);
    h.age_attachment(d, 600).await;
    assert_eq!(h.app.reaper_scan().await.unwrap(), 0);
}
