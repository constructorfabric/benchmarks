//! US5: attachments.
//!
//! AC: Attachments (lifecycle + validation, async indexing incl. failure/timeout, tool
//! availability, cleanup + abandoned uploads), Context Assembly (tool guidance), isolation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

fn id_of(r: &Resp) -> Uuid {
    Uuid::parse_str(r.json()["id"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn document_upload_reaches_ready_with_vector_store() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let r = h.upload(ALICE, chat, "notes.md", "text/markdown", b"# Notes\nhello").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let a = r.json();
    assert_eq!(a["status"], json!("ready"));
    assert_eq!(a["kind"], json!("document"));
    assert_eq!(a["filename"], json!("notes.md"));
    assert_eq!(a["content_type"], json!("text/markdown"));
    assert_eq!(a["size_bytes"], json!(13));
    assert!(a.get("error_code").is_none());
    assert!(!a.to_string().contains("file-"), "no provider id exposed");
    let id = id_of(&r);

    // Provider requests: file upload, vector store create, add file.
    assert_eq!(h.provider.count("POST", "/files"), 2, "{:?}", h.provider.recorded().iter().map(|r| r.path.clone()).collect::<Vec<_>>());
    assert_eq!(h.provider.count("POST", "/vector_stores"), 2);
    let add = h.provider.recorded().into_iter().find(|r| r.path.contains("/vector_stores/") && r.method == "POST").unwrap();
    let body = add.json.unwrap();
    assert!(body["file_id"].as_str().unwrap().starts_with("file-"));
    assert_eq!(body["attributes"]["attachment_id"], json!(id.to_string()), "{body}");

    // A second document reuses the chat vector store.
    let r = h.upload(ALICE, chat, "b.txt", "text/plain", b"more").await;
    assert_eq!(r.status, 201);
    assert_eq!(h.provider.recorded().iter().filter(|r| r.method == "POST" && r.path.ends_with("/vector_stores")).count(), 1);

    // DB row
    let row = h.attachment(id).await;
    assert_eq!(row.status, "ready");
    assert!(row.for_file_search);
    assert!(row.provider_file_id.is_some());
    assert_eq!(row.uploaded_by_user_id, ALICE.user);

    // GET
    let g = h.send(ALICE, "GET", &format!("/chats/{chat}/attachments/{id}"), None).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["id"], json!(id.to_string()));
    assert_eq!(g.json()["status"], json!("ready"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn image_upload_has_thumbnail_and_no_vector_store() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let r = h.upload(ALICE, chat, "pic.png", "image/png", &png(300, 200)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let a = r.json();
    assert_eq!(a["kind"], json!("image"));
    assert_eq!(a["status"], json!("ready"));
    let t = &a["img_thumbnail"];
    assert_eq!(t["content_type"], json!("image/webp"), "{a}");
    assert!(t["width"].as_u64().unwrap() <= 128 && t["height"].as_u64().unwrap() <= 128, "{t}");
    assert!(t["data_base64"].as_str().is_some_and(|s| !s.is_empty()), "{t}");
    assert_eq!(h.provider.count("POST", "/vector_stores"), 0);
    // octet-stream with an image extension is classified as an image
    let r = h.upload(ALICE, chat, "x.jpg", "application/octet-stream", &png(2, 2)).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["content_type"], json!("image/jpeg"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn xlsx_goes_to_code_interpreter_and_respects_kill_switch_and_capability() {
    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let r = h.upload(ALICE, chat, "t.xlsx", XLSX, b"PK fake").await;
    assert_eq!(r.status, 201, "{}", r.text());
    let row = h.attachment(id_of(&r)).await;
    assert!(row.for_code_interpreter);
    assert!(!row.for_file_search);
    assert_eq!(h.provider.count("POST", "/vector_stores"), 0, "xlsx is not indexed");
    // The provider request carries code_interpreter with the file.
    h.say(ALICE, chat, "sum column A").await;
    let req = h.provider.chat_requests().pop().unwrap();
    let ci = req["tools"].as_array().unwrap().iter().find(|t| t["type"] == json!("code_interpreter")).expect("code_interpreter tool").clone();
    assert_eq!(ci["container"]["file_ids"].as_array().unwrap().len(), 1);

    // Model without code interpreter support → CODE_INTERPRETER_UNAVAILABLE.
    let chat2 = h.chat(ALICE, Some("novision-m")).await;
    let r = h.upload(ALICE, chat2, "t.xlsx", XLSX, b"PK").await;
    assert_eq!(fv_reason(&r.problem(400)), "CODE_INTERPRETER_UNAVAILABLE");

    // Kill switch.
    h.policy.set(&policy_cfg(catalog(), json!({"kill_switches": {"disable_code_interpreter": true}})));
    let r = h.upload(ALICE, chat, "t2.xlsx", XLSX, b"PK").await;
    assert_eq!(fv_reason(&r.problem(400)), "CODE_INTERPRETER_UNAVAILABLE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_validation_errors() {
    let mut opts = Opts::default();
    opts.cfg.rag.uploaded_file_max_size_kb = 1;
    opts.cfg.rag.uploaded_image_max_size_kb = 1;
    let h = Harness::with(opts).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;

    let r = h.upload(ALICE, chat, "big.txt", "text/plain", &vec![b'a'; 2048]).await;
    let p = r.problem(400);
    assert_eq!(fv_reason(&p), "FILE_TOO_LARGE");
    assert_eq!(p["context"]["field_violations"][0]["field"], json!("content_length"));
    let r = h.upload(ALICE, chat, "big.png", "image/png", &vec![0u8; 2048]).await;
    assert_eq!(fv_reason(&r.problem(400)), "FILE_TOO_LARGE");

    let r = h.upload(ALICE, chat, "x.exe", "application/x-msdownload", b"MZ").await;
    assert_eq!(fv_reason(&r.problem(400)), "UNSUPPORTED_CONTENT_TYPE");

    // multipart errors
    let r = h.raw(ALICE, "POST", &format!("/chats/{chat}/attachments"), Some("multipart/form-data"), b"x".to_vec()).await;
    assert_eq!(fv_reason(&r.problem(400)), "BOUNDARY_REQUIRED");
    let body = b"--B\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nv\r\n--B--\r\n".to_vec();
    let r = h.raw(ALICE, "POST", &format!("/chats/{chat}/attachments"), Some("multipart/form-data; boundary=B"), body).await;
    assert_eq!(fv_reason(&r.problem(400)), "MISSING_FILE");
    let body = b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\nv\r\n--B--\r\n".to_vec();
    let r = h.raw(ALICE, "POST", &format!("/chats/{chat}/attachments"), Some("multipart/form-data; boundary=B"), body).await;
    assert_eq!(fv_reason(&r.problem(400)), "MISSING_CONTENT_TYPE");
    let r = h.raw(ALICE, "POST", &format!("/chats/{chat}/attachments"), Some("multipart/form-data; boundary=B"), b"garbage".to_vec()).await;
    r.problem(400);

    // Nothing reached the provider, no rows besides none.
    assert_eq!(h.provider.count("POST", "/files"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_chat_document_and_storage_limits() {
    let mut opts = Opts::default();
    opts.cfg.rag.max_documents_per_chat = 2;
    opts.cfg.rag.max_total_upload_mb_per_chat = 1;
    let h = Harness::with(opts).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    for i in 0..2 {
        let r = h.upload(ALICE, chat, &format!("{i}.txt"), "text/plain", b"doc").await;
        assert_eq!(r.status, 201);
    }
    let r = h.upload(ALICE, chat, "3.txt", "text/plain", b"doc").await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["subject"], json!("document_limit"), "{p}");

    let chat2 = h.chat(ALICE, Some("standard-m")).await;
    let r = h.upload(ALICE, chat2, "a.txt", "text/plain", &vec![b'a'; 700 * 1024]).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let r = h.upload(ALICE, chat2, "b.txt", "text/plain", &vec![b'a'; 400 * 1024]).await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["subject"], json!("storage_limit"), "{p}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_upload_failure_is_503_and_failed_row() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    *h.provider.file_upload_status.lock().unwrap() = 500;
    let r = h.upload(ALICE, chat, "a.txt", "text/plain", b"x").await;
    let p = r.problem(503);
    assert_eq!(r.headers["retry-after"], "10");
    assert!(!p.to_string().contains("upload_failed"));
    // The failed attachment is persisted with its error code.
    let all = attachments_of(&h, chat).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, "failed");
    assert_eq!(all[0].error_code.as_deref(), Some("upload_failed"));
    let g = h.send(ALICE, "GET", &format!("/chats/{chat}/attachments/{}", all[0].id), None).await;
    assert_eq!(g.json()["status"], json!("failed"));
    assert_eq!(g.json()["error_code"], json!("upload_failed"));
}

async fn attachments_of(h: &Harness, chat: Uuid) -> Vec<ent::attachments::Model> {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    use toolkit_db::secure::SecureEntityExt;
    ent::attachments::Entity::find()
        .filter(ent::attachments::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&toolkit_security::AccessScope::allow_all())
        .all(&h.db.conn().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indexing_failure_is_503_and_deletes_provider_file() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    *h.provider.vs_add_status.lock().unwrap() = "failed".into();
    let r = h.upload(ALICE, chat, "a.txt", "text/plain", b"x").await;
    r.problem(503);
    assert_eq!(r.headers["retry-after"], "10");
    let all = attachments_of(&h, chat).await;
    assert_eq!(all[0].status, "failed");
    assert_eq!(all[0].error_code.as_deref(), Some("indexing_failed"));
    h.eventually("provider file delete", || h.provider.count("DELETE", "/files/") == 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indexing_in_progress_polls_until_completed() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    *h.provider.vs_add_status.lock().unwrap() = "in_progress".into();
    h.provider.vs_poll_statuses.lock().unwrap().extend(["in_progress".to_owned(), "completed".to_owned()]);
    let r = h.upload(ALICE, chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["status"], json!("ready"));
    assert_eq!(h.provider.count("GET", "/vector_stores/"), 2);
}

/// Indexing still in progress at the 25 s deadline: 201 `uploaded`, background completion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indexing_deadline_returns_uploaded_and_completes_in_background() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    *h.provider.vs_add_status.lock().unwrap() = "in_progress".into();
    h.provider.vs_poll_statuses.lock().unwrap().extend(std::iter::repeat_n("in_progress".to_owned(), 30));
    let began = std::time::Instant::now();
    let r = h.upload(ALICE, chat, "slow.txt", "text/plain", b"x").await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["status"], json!("uploaded"));
    assert!(began.elapsed() < Duration::from_secs(30));
    let id = id_of(&r);
    // A message referencing a non-ready attachment is rejected.
    let s = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x", "attachment_ids": [id]}))).await;
    assert_eq!(fv_reason(&s.problem(400)), "invalid_attachment");
    // Background poller sees `completed`.
    h.provider.vs_poll_statuses.lock().unwrap().clear();
    for _ in 0..100 {
        if h.attachment(id).await.status == "ready" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(h.attachment(id).await.status, "ready");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_lifecycle_lock_and_idempotency() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let doc = id_of(&h.upload(ALICE, chat, "a.txt", "text/plain", b"x").await);
    let img = id_of(&h.upload(ALICE, chat, "a.png", "image/png", &png(3, 3)).await);

    // Unreferenced: delete → 204, then GET 404, repeated delete 204 (idempotent).
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/attachments/{doc}"), None).await;
    assert_eq!(r.status, 204, "{}", r.text());
    h.send(ALICE, "GET", &format!("/chats/{chat}/attachments/{doc}"), None).await.problem(404);
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/attachments/{doc}"), None).await;
    assert_eq!(r.status, 204);
    let row = h.attachment(doc).await;
    assert!(row.deleted_at.is_some());
    // Cleanup deletes the provider file and removes it from the vector store.
    h.eventually("provider file delete", || h.provider.count("DELETE", "/files/") >= 1).await;
    for _ in 0..100 {
        if h.attachment(doc).await.cleanup_status.as_deref() == Some("done") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.attachment(doc).await.cleanup_status.as_deref(), Some("done"));

    // Referenced by a message → 409 attachment_locked.
    h.stream(ALICE, chat, json!({"content": "see", "attachment_ids": [img]})).await;
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/attachments/{img}"), None).await;
    let p = r.problem(409);
    assert_eq!(p["context"]["resource_name"], json!("attachment_locked"), "{p}");
    // Unknown attachment → 404.
    h.send(ALICE, "DELETE", &format!("/chats/{chat}/attachments/{}", Uuid::new_v4()), None).await.problem(404);
    // After the referencing turn is deleted the image can be deleted.
    let rid = h.turns(chat).await[0].request_id;
    assert_eq!(h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await.status, 204);
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/attachments/{img}"), None).await;
    assert_eq!(r.status, 204, "{}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachments_are_private_to_the_uploader() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let id = id_of(&h.upload(ALICE, chat, "a.txt", "text/plain", b"x").await);
    for who in [BOB, CAROL] {
        h.send(who, "GET", &format!("/chats/{chat}/attachments/{id}"), None).await.problem(404);
    }
    // Another chat of the same user does not expose it either.
    let other = h.chat(ALICE, Some("standard-m")).await;
    h.send(ALICE, "GET", &format!("/chats/{other}/attachments/{id}"), None).await.problem(404);
    let r = h.send(ALICE, "POST", &format!("/chats/{other}/messages:stream"), Some(json!({"content": "x", "attachment_ids": [id]}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "invalid_attachment");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tools_and_images_in_provider_request() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    // No documents: no file_search.
    h.say(ALICE, chat, "plain").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req.get("tools").is_none_or(|t| t.as_array().unwrap().is_empty()));
    assert!(!req["instructions"].as_str().unwrap().contains(mini_chat::config::DEFAULT_FILE_SEARCH_GUARD));

    let doc = id_of(&h.upload(ALICE, chat, "spec.pdf", "application/pdf", b"%PDF").await);
    let img = id_of(&h.upload(ALICE, chat, "p.png", "image/png", &png(5, 5)).await);
    let file_id = h.attachment(doc).await.provider_file_id.unwrap();
    let img_file = h.attachment(img).await.provider_file_id.unwrap();
    h.provider.push(Reply::with_events(
        "From the spec",
        vec![
            ("response.file_search_call.searching", json!({"type": "response.file_search_call.searching"})),
            ("response.file_search_call.completed", json!({"type": "response.file_search_call.completed"})),
        ],
        json!({"input_tokens": 10, "output_tokens": 5}),
        vec![json!({"type": "file_citation", "file_id": file_id, "filename": format!("{chat}_{doc}.pdf"), "index": 0})],
    ));
    let (r, ev) = h.stream(ALICE, chat, json!({"content": "what does the spec say?", "attachment_ids": [img]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let req = h.provider.chat_requests().pop().unwrap();
    let tools = req["tools"].as_array().unwrap();
    let fs = tools.iter().find(|t| t["type"] == json!("file_search")).expect("file_search tool");
    assert_eq!(fs["vector_store_ids"].as_array().unwrap().len(), 1);
    assert_eq!(fs["max_num_results"], json!(5));
    assert!(req["instructions"].as_str().unwrap().contains(mini_chat::config::DEFAULT_FILE_SEARCH_GUARD), "file_search guard added");
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    let parts = last["content"].as_array().unwrap();
    assert!(parts.iter().any(|p| p["type"] == json!("input_image") && p["file_id"] == json!(img_file)), "{last}");
    assert_eq!(req["metadata"]["feature"], json!("file_search"));

    // tool events and citations
    let tool_events: Vec<&Value> = ev.iter().filter(|(n, _)| n == "tool").map(|(_, d)| d).collect();
    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0]["phase"], json!("start"));
    assert_eq!(tool_events[0]["name"], json!("file_search"));
    assert_eq!(tool_events[1]["phase"], json!("done"));
    let n = names(&ev);
    let ci = n.iter().position(|e| *e == "citations").expect("citations");
    assert_eq!(ci, n.len() - 2, "citations right before done: {n:?}");
    let items = find(&ev, "citations")["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["source"], json!("file"));
    assert_eq!(items[0]["attachment_id"], json!(doc.to_string()));
    assert_eq!(items[0]["title"], json!("spec.pdf"));
    assert!(!find(&ev, "citations").to_string().contains("file-"), "provider file id never exposed");
    let turn = h.turns(chat).await.pop().unwrap();
    assert_eq!(turn.file_search_completed_count, 1);

    // Images on a non-vision model → VISION_NOT_SUPPORTED; kill switch → FEATURE_DISABLED.
    let nv = h.chat(ALICE, Some("novision-m")).await;
    let nimg = id_of(&h.upload(ALICE, nv, "q.png", "image/png", &png(2, 2)).await);
    let r = h.send(ALICE, "POST", &format!("/chats/{nv}/messages:stream"), Some(json!({"content": "x", "attachment_ids": [nimg]}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "VISION_NOT_SUPPORTED");
    h.policy.set(&policy_cfg(catalog(), json!({"kill_switches": {"disable_images": true, "disable_file_search": true}})));
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x", "attachment_ids": [img]}))).await;
    let p = r.problem(400);
    assert_eq!(p["context"]["violations"][0]["type"], json!("FEATURE_DISABLED"), "{p}");
    assert_eq!(p["context"]["violations"][0]["subject"], json!("images"));
    let r = h.upload(ALICE, chat, "z.png", "image/png", &png(2, 2)).await;
    assert_eq!(r.problem(400)["context"]["violations"][0]["type"], json!("FEATURE_DISABLED"));
    // file_search kill switch removes the tool.
    h.say(ALICE, chat, "no tools now").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req.get("tools").is_none_or(|t| !t.to_string().contains("file_search")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_many_images_in_one_message() {
    let mut opts = Opts::default();
    opts.cfg.rag.max_images_per_message = 1;
    let h = Harness::with(opts).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let a = id_of(&h.upload(ALICE, chat, "a.png", "image/png", &png(2, 2)).await);
    let b = id_of(&h.upload(ALICE, chat, "b.png", "image/png", &png(2, 2)).await);
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x", "attachment_ids": [a, b]}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "TOO_MANY_IMAGES");
    assert!(h.provider.chat_requests().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_reaper_fails_abandoned_uploads() {
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    use toolkit_db::secure::SecureUpdateExt;

    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    *h.provider.vs_add_status.lock().unwrap() = "failed".into();
    let id = id_of(&h.upload(ALICE, chat, "ok.png", "image/png", &png(2, 2)).await);
    // Simulate an upload stuck in `uploaded` (process died mid-upload).
    let old = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
    ent::attachments::Entity::update_many()
        .col_expr(ent::attachments::Column::Status, sea_orm::sea_query::Expr::value("uploaded"))
        .col_expr(ent::attachments::Column::UpdatedAt, sea_orm::sea_query::Expr::value(old))
        .filter(ent::attachments::Column::Id.eq(id))
        .secure()
        .scope_with(&toolkit_security::AccessScope::allow_all())
        .exec(&h.db.conn().unwrap())
        .await
        .unwrap();
    // Fresh rows are left alone.
    let fresh = id_of(&h.upload(ALICE, chat, "fresh.png", "image/png", &png(2, 2)).await);
    assert_eq!(h.svc.reaper_scan().await.unwrap(), 1);
    let row = h.attachment(id).await;
    assert_eq!(row.status, "failed");
    assert_eq!(row.error_code.as_deref(), Some("upload_abandoned"));
    assert_eq!(h.attachment(fresh).await.status, "ready");
    // The provider file is cleaned up through the outbox.
    h.eventually("provider file cleanup", || h.provider.count("DELETE", "/files/") >= 1).await;
    assert_eq!(h.svc.reaper_scan().await.unwrap(), 0);
}
