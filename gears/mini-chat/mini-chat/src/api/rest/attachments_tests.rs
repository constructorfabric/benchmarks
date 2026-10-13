//! Router tests: attachment upload / get / delete, indexing lifecycle, provider
//! tools, image guards, citations, cleanup and the upload reaper (acceptance:
//! Attachments, Cleanup & Recovery).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::{
    EnvOptions, TENANT_A, TestEnv, TestResponse, USER_A2, ctx, default_catalog, json_resp, sse_resp, user_a,
};

const BOUNDARY: &str = "XBOUNDARYX";

struct Part<'a> {
    name: &'a str,
    filename: Option<&'a str>,
    content_type: Option<&'a str>,
    data: Vec<u8>,
}

fn multipart(parts: &[Part<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let mut disp = format!("Content-Disposition: form-data; name=\"{}\"", p.name);
        if let Some(f) = p.filename {
            write!(disp, "; filename=\"{f}\"").unwrap();
        }
        out.extend_from_slice(disp.as_bytes());
        out.extend_from_slice(b"\r\n");
        if let Some(ct) = p.content_type {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&p.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

async fn upload_raw(env: &TestEnv, chat: &str, content_type: &str, body: Vec<u8>) -> TestResponse {
    let mut req = Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut().insert(user_a());
    env.send(req).await
}

async fn upload(env: &TestEnv, chat: &str, filename: &str, ct: &str, data: Vec<u8>) -> TestResponse {
    let body = multipart(&[Part {
        name: "file",
        filename: Some(filename),
        content_type: Some(ct),
        data,
    }]);
    upload_raw(env, chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 255) as u8, (y % 255) as u8, 128]));
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

fn pdf() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n".to_vec()
}

fn id_of(r: &TestResponse) -> String {
    assert_eq!(r.status, 201, "{}", r.text());
    r.json()["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn document_upload_indexes_and_feeds_file_search() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let r = upload(&env, &chat, "report.pdf", "application/pdf", pdf()).await;
    let att = id_of(&r);
    let v = r.json();
    assert_eq!(v["status"], "ready");
    assert_eq!(v["kind"], "document");
    assert_eq!(v["filename"], "report.pdf");
    assert_eq!(v["content_type"], "application/pdf");
    assert_eq!(v["size_bytes"], pdf().len());
    for absent in ["img_thumbnail", "error_code", "doc_summary", "summary_updated_at"] {
        assert!(v.get(absent).is_none(), "{absent} must be omitted: {v}");
    }
    assert!(!r.text().contains("file-0"), "no provider ids in the API");
    // provider calls: file upload (assistants purpose), vector store create, add file
    let files = env.proxy.requests_to("/v1/files");
    let up = files.iter().find(|x| x.method == "POST").unwrap();
    assert!(up.content_type.starts_with("multipart/form-data"));
    assert!(String::from_utf8_lossy(&up.body).contains("assistants"));
    assert!(env.proxy.requests_to("/v1/vector_stores").iter().any(|x| x.method == "POST" && x.uri.ends_with("/v1/vector_stores")));
    assert!(env.proxy.requests_to("/files").iter().any(|x| x.uri.contains("/vector_stores/vs_")));
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_vector_stores WHERE vector_store_id = 'vs_abcdefghijklmnop'").await, 1);
    // GET returns the same detail
    let g = env.get(&format!("/mini-chat/v1/chats/{chat}/attachments/{att}")).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["status"], "ready");
    // the next message gets file_search over the chat vector store, and the summary
    let r = env.stream(&user_a(), &chat, json!({"content": "summarize", "attachment_ids": [att]})).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let req = env.proxy.chat_requests().last().unwrap().json();
    let tools = req["tools"].as_array().unwrap();
    let fs = tools.iter().find(|t| t["type"] == "file_search").expect("file_search tool");
    assert_eq!(fs["vector_store_ids"], json!(["vs_abcdefghijklmnop"]));
    assert_eq!(req["metadata"]["feature"], "file_search");
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let a = &msgs["items"][0]["attachments"][0];
    assert_eq!(a["attachment_id"], att.as_str());
    assert_eq!(a["kind"], "document");
    assert_eq!(a["filename"], "report.pdf");
    assert_eq!(a["status"], "ready");
    assert!(a.get("img_thumbnail").is_none());
    // a referenced attachment is locked
    let r = env.call(&user_a(), "DELETE", &format!("/mini-chat/v1/chats/{chat}/attachments/{att}"), None).await;
    r.assert_problem(409, "attachment_locked");
    // a second chat message without attachment_ids still gets file_search (whole store)
    let r = env.send_msg(&chat, "again").await;
    assert_eq!(r.event_names().last().unwrap(), "done");
    let req = env.proxy.chat_requests().last().unwrap().json();
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["type"] == "file_search"));
}

#[tokio::test]
async fn image_upload_thumbnail_and_multimodal_input() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let r = upload(&env, &chat, "pic.png", "image/png", png(640, 480)).await;
    let att = id_of(&r);
    let v = r.json();
    assert_eq!(v["kind"], "image");
    assert_eq!(v["status"], "ready");
    let t = &v["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp");
    assert!(t["width"].as_u64().unwrap() <= 128 * 4);
    assert!(t["height"].as_u64().unwrap() >= 1);
    assert!(!t["data_base64"].as_str().unwrap().is_empty());
    assert!(env.proxy.requests_to("/v1/vector_stores").is_empty(), "images never go to the vector store");
    let r = env.stream(&user_a(), &chat, json!({"content": "what is this", "attachment_ids": [att]})).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let req = env.proxy.chat_requests().last().unwrap().json();
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    let parts = last["content"].as_array().expect("multimodal content parts");
    assert!(parts.iter().any(|p| p["type"] == "input_image" && p["file_id"].as_str().unwrap().starts_with("file-")));
    assert!(parts.iter().any(|p| p["type"] == "input_text" && p["text"] == "what is this"));
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert!(msgs["items"][0]["attachments"][0]["img_thumbnail"]["data_base64"].is_string());
    // images from earlier turns are not implicitly reused
    env.send_msg(&chat, "and now?").await;
    let req = env.proxy.chat_requests().last().unwrap().json();
    let s = req["input"].to_string();
    assert_eq!(s.matches("input_image").count(), 0, "history carries no images: {s}");
}

#[tokio::test]
async fn image_guards() {
    let env = TestEnv::with(EnvOptions {
        config: json!({"rag": {"max_images_per_message": 1}}),
        ..Default::default()
    })
    .await;
    let chat = env.chat(None).await;
    let a = id_of(&upload(&env, &chat, "a.png", "image/png", png(8, 8)).await);
    let b = id_of(&upload(&env, &chat, "b.png", "image/png", png(8, 8)).await);
    let r = env.stream(&user_a(), &chat, json!({"content": "x", "attachment_ids": [a, b]})).await;
    r.assert_problem(400, "TOO_MANY_IMAGES");
    // a model without vision
    let nv = env.chat(Some("no-vision")).await;
    let c = id_of(&upload(&env, &nv, "c.png", "image/png", png(8, 8)).await);
    let r = env.stream(&user_a(), &nv, json!({"content": "x", "attachment_ids": [c]})).await;
    r.assert_problem(400, "VISION_NOT_SUPPORTED");
    // an attachment of another chat is invalid
    let r = env.stream(&user_a(), &nv, json!({"content": "x", "attachment_ids": [a]})).await;
    r.assert_problem(400, "invalid_attachment");
    assert!(env.proxy.chat_requests().is_empty());
}

#[tokio::test]
async fn disable_images_kill_switch() {
    let env = TestEnv::with(EnvOptions {
        policy: json!({"model_catalog": default_catalog(), "kill_switches": {"disable_images": true}}),
        ..Default::default()
    })
    .await;
    let chat = env.chat(None).await;
    let r = upload(&env, &chat, "a.png", "image/png", png(8, 8)).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.json()["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "images");
}

#[tokio::test]
async fn upload_validation_errors() {
    let env = TestEnv::with(EnvOptions {
        config: json!({"rag": {"uploaded_file_max_size_kb": 1, "max_documents_per_chat": 1}}),
        ..Default::default()
    })
    .await;
    let chat = env.chat(None).await;
    let r = upload_raw(&env, &chat, "multipart/form-data", b"x".to_vec()).await;
    r.assert_problem(400, "BOUNDARY_REQUIRED");
    let r = upload_raw(&env, &chat, &format!("multipart/form-data; boundary={BOUNDARY}"), b"garbage".to_vec()).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let body = multipart(&[Part { name: "other", filename: None, content_type: None, data: b"x".to_vec() }]);
    let r = upload_raw(&env, &chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await;
    r.assert_problem(400, "MISSING_FILE");
    let body = multipart(&[Part { name: "file", filename: Some("a.pdf"), content_type: None, data: pdf() }]);
    let r = upload_raw(&env, &chat, &format!("multipart/form-data; boundary={BOUNDARY}"), body).await;
    r.assert_problem(400, "MISSING_CONTENT_TYPE");
    let r = upload(&env, &chat, "a.bin", "application/x-unknown", b"abc".to_vec()).await;
    r.assert_problem(400, "UNSUPPORTED_CONTENT_TYPE");
    let r = upload(&env, &chat, "a.weird", "application/octet-stream", b"abc".to_vec()).await;
    r.assert_problem(400, "UNSUPPORTED_CONTENT_TYPE");
    let r = upload(&env, &chat, "big.pdf", "application/pdf", vec![b'a'; 2048]).await;
    r.assert_problem(400, "FILE_TOO_LARGE");
    assert_eq!(r.json()["context"]["field_violations"][0]["field"], "content_length");
    // octet-stream with a known extension is inferred
    let r = upload(&env, &chat, "doc.pdf", "application/octet-stream", pdf()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["content_type"], "application/pdf");
    // per-chat document limit
    let r = upload(&env, &chat, "doc2.pdf", "application/pdf", pdf()).await;
    r.assert_problem(429, "document_limit");
    // unknown chat, foreign user
    let r = upload(&env, &Uuid::new_v4().to_string(), "doc.pdf", "application/pdf", pdf()).await;
    r.assert_problem(404, "gts.cf.core.mini_chat.chat.v1~");
    // chat model removed from the catalog
    env.sql_exec(&format!("UPDATE chats SET model = 'ghost' WHERE id = x'{}'", chat.replace('-', ""))).await;
    let r = upload(&env, &chat, "doc.pdf", "application/pdf", pdf()).await;
    r.assert_problem(400, "INVALID_MODEL");
}

#[tokio::test]
async fn code_interpreter_only_file_needs_code_interpreter() {
    let env = TestEnv::new().await;
    let chat = env.chat(Some("no-vision")).await;
    let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let r = upload(&env, &chat, "t.xlsx", xlsx, b"PK\x03\x04data".to_vec()).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert_eq!(r.json()["title"].as_str().unwrap().to_lowercase().replace(' ', "_"), "invalid_argument");
    // with code interpreter the xlsx is uploaded and offered to code_interpreter
    let chat = env.chat(None).await;
    let att = id_of(&upload(&env, &chat, "t.xlsx", xlsx, b"PK\x03\x04data".to_vec()).await);
    let r = env.stream(&user_a(), &chat, json!({"content": "analyze", "attachment_ids": [att]})).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let req = env.proxy.chat_requests().last().unwrap().json();
    let ci = req["tools"].as_array().unwrap().iter().find(|t| t["type"] == "code_interpreter").cloned();
    let ci = ci.expect("code_interpreter tool");
    assert!(ci.to_string().contains("file-"), "{ci}");
}

#[tokio::test]
async fn indexing_failure_returns_503_and_marks_failed() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    env.proxy.respond(|r| {
        (r.method == "POST" && r.uri.contains("/vector_stores/") && r.uri.ends_with("/files"))
            .then(|| json_resp(200, &json!({"status": "failed"})))
    });
    let r = upload(&env, &chat, "a.pdf", "application/pdf", pdf()).await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert_eq!(r.headers["retry-after"], "10");
    assert!(!r.text().contains("indexing_failed"));
    let rows = env.sql_rows("SELECT hex(id), status, error_code FROM attachments").await;
    assert_eq!(rows[0].try_get_by_index::<String>(1).unwrap(), "failed");
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(2).unwrap().as_deref(), Some("indexing_failed"));
    let id = Uuid::parse_str(&rows[0].try_get_by_index::<String>(0).unwrap()).unwrap();
    let g = env.get(&format!("/mini-chat/v1/chats/{chat}/attachments/{id}")).await.json();
    assert_eq!(g["status"], "failed");
    assert_eq!(g["error_code"], "indexing_failed");
    // provider file deleted best effort
    env.eventually("file delete", |e| e.proxy.requests_to("/v1/files/file-").iter().any(|x| x.method == "DELETE")).await;
    // provider upload failure
    env.proxy.respond(|r| (r.method == "POST" && r.uri.ends_with("/v1/files")).then(|| json_resp(500, &json!({}))));
    let r = upload(&env, &chat, "b.pdf", "application/pdf", pdf()).await;
    assert_eq!(r.status, 503);
}

#[tokio::test]
async fn slow_indexing_returns_uploaded_then_background_ready() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let done = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&done);
    env.proxy.respond(move |r| {
        if r.uri.contains("/vector_stores/") && r.uri.contains("/files") {
            let status = if d2.load(Ordering::SeqCst) { "completed" } else { "in_progress" };
            return Some(json_resp(200, &json!({"status": status})));
        }
        None
    });
    let started = std::time::Instant::now();
    let r = upload(&env, &chat, "a.pdf", "application/pdf", pdf()).await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(r.json()["status"], "uploaded");
    assert!(started.elapsed() >= std::time::Duration::from_secs(24));
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
    let att = r.json()["id"].as_str().unwrap().to_owned();
    // an uploaded (not ready) attachment cannot be referenced yet
    let s = env.stream(&user_a(), &chat, json!({"content": "x", "attachment_ids": [att]})).await;
    s.assert_problem(400, "invalid_attachment");
    done.store(true, Ordering::SeqCst);
    let mut status = Value::Null;
    for _ in 0..200 {
        status = env.get(&format!("/mini-chat/v1/chats/{chat}/attachments/{att}")).await.json()["status"].clone();
        if status == "ready" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(status, "ready");
}

#[tokio::test]
async fn get_delete_ownership_and_cleanup() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let att = id_of(&upload(&env, &chat, "a.pdf", "application/pdf", pdf()).await);
    let uri = format!("/mini-chat/v1/chats/{chat}/attachments/{att}");
    // foreign user / other chat / unknown
    let r = env.call(&ctx(TENANT_A, USER_A2), "GET", &uri, None).await;
    assert_eq!(r.status, 404);
    let other = env.chat(None).await;
    let r = env.get(&format!("/mini-chat/v1/chats/{other}/attachments/{att}")).await;
    r.assert_problem(404, "attachment");
    let r = env.get(&format!("/mini-chat/v1/chats/{chat}/attachments/{}", Uuid::new_v4())).await;
    r.assert_problem(404, "attachment");
    // uploaded by another user in the caller's chat → 404 for GET and DELETE
    env.sql_exec(&format!("UPDATE attachments SET uploaded_by_user_id = x'{}'", USER_A2.simple())).await;
    assert_eq!(env.get(&uri).await.status, 404);
    assert_eq!(env.call(&user_a(), "DELETE", &uri, None).await.status, 404);
    env.sql_exec(&format!("UPDATE attachments SET uploaded_by_user_id = x'{}'", crate::test_support::USER_A.simple())).await;
    // delete: 204, idempotent, then hidden; provider cleanup via outbox
    assert_eq!(env.call(&user_a(), "DELETE", &uri, None).await.status, 204);
    assert_eq!(env.call(&user_a(), "DELETE", &uri, None).await.status, 204);
    assert_eq!(env.get(&uri).await.status, 404);
    env.eventually("provider file delete", |e| {
        e.proxy.requests_to("/v1/files/file-").iter().any(|x| x.method == "DELETE")
    })
    .await;
}

#[tokio::test]
async fn chat_deletion_cleans_provider_resources() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    id_of(&upload(&env, &chat, "a.pdf", "application/pdf", pdf()).await);
    id_of(&upload(&env, &chat, "b.png", "image/png", png(8, 8)).await);
    let r = env.call(&user_a(), "DELETE", &format!("/mini-chat/v1/chats/{chat}"), None).await;
    assert_eq!(r.status, 204);
    env.eventually("vector store delete", |e| {
        e.proxy.requests_to("/v1/vector_stores/vs_").iter().any(|x| x.method == "DELETE")
    })
    .await;
    env.eventually("file deletes", |e| {
        e.proxy.requests_to("/v1/files/file-").iter().filter(|x| x.method == "DELETE").count() >= 2
    })
    .await;
    // provider 404 counts as deleted; rows are kept (soft delete only)
    assert_eq!(env.count("SELECT COUNT(*) FROM attachments").await, 2);
}

#[tokio::test]
async fn upload_reaper_marks_abandoned_rows() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let att = id_of(&upload(&env, &chat, "a.pdf", "application/pdf", pdf()).await);
    let hex = att.replace('-', "");
    env.sql_exec(&format!(
        "UPDATE attachments SET status = 'uploaded', updated_at = '2020-01-01T00:00:00.000001Z' WHERE id = x'{hex}'"
    ))
    .await;
    env.svc.reaper_scan().await.unwrap();
    let rows = env.sql_rows(&format!("SELECT status, error_code FROM attachments WHERE id = x'{hex}'")).await;
    assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "failed");
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(1).unwrap().as_deref(), Some("upload_abandoned"));
    env.eventually("abandoned file delete", |e| {
        e.proxy.requests_to("/v1/files/file-").iter().any(|x| x.method == "DELETE")
    })
    .await;
    // fresh rows are untouched
    let fresh = id_of(&upload(&env, &chat, "b.pdf", "application/pdf", pdf()).await);
    env.sql_exec(&format!("UPDATE attachments SET status = 'pending' WHERE id = x'{}'", fresh.replace('-', ""))).await;
    env.svc.reaper_scan().await.unwrap();
    let g = env.get(&format!("/mini-chat/v1/chats/{chat}/attachments/{fresh}")).await.json();
    assert_eq!(g["status"], "pending");
}

#[tokio::test]
async fn file_citations_map_to_attachments_without_provider_ids() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let att = id_of(&upload(&env, &chat, "manual.pdf", "application/pdf", pdf()).await);
    let file_id = env
        .sql_rows(&format!("SELECT provider_file_id FROM attachments WHERE id = x'{}'", att.replace('-', "")))
        .await[0]
        .try_get_by_index::<String>(0)
        .unwrap();
    let fid = file_id.clone();
    env.proxy.respond(move |r| {
        r.uri.contains("/responses").then(|| {
            sse_resp(&[
                ("response.file_search_call.in_progress".into(), json!({"item_id": "fs_1"})),
                ("response.file_search_call.completed".into(), json!({"item_id": "fs_1"})),
                ("response.output_text.delta".into(), json!({"delta": "See manual"})),
                (
                    "response.output_text.annotation.added".into(),
                    json!({"annotation": {"type": "file_citation", "file_id": fid, "filename": "manual.pdf", "index": 3}}),
                ),
                (
                    "response.output_text.annotation.added".into(),
                    json!({"annotation": {"type": "file_citation", "file_id": "file-unknownunknown1", "filename": "x", "index": 3}}),
                ),
                ("response.completed".into(), json!({"response": {"usage": {"input_tokens": 5, "output_tokens": 2}}})),
            ])
        })
    });
    let r = env.send_msg(&chat, "where?").await;
    let names = r.event_names();
    assert!(names.contains(&"tool".to_owned()), "{names:?}");
    let ci = names.iter().position(|n| n == "citations").expect("citations event");
    assert_eq!(names.last().unwrap(), "done");
    assert!(ci < names.len() - 1, "citations precede done");
    let c = r.event("citations").unwrap();
    let items = c["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "unknown provider files are dropped: {c}");
    assert_eq!(items[0]["source"], "file");
    assert_eq!(items[0]["attachment_id"], att.as_str());
    assert_eq!(items[0]["title"], "manual.pdf");
    assert!(!r.text().contains(&file_id), "provider file id leaked");
    let tool = r.event("tool").unwrap();
    assert_eq!(tool["name"], "file_search");
}

#[tokio::test]
async fn retry_and_edit_carry_attachments_forward() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let doc = id_of(&upload(&env, &chat, "a.pdf", "application/pdf", pdf()).await);
    let img = id_of(&upload(&env, &chat, "b.png", "image/png", png(8, 8)).await);
    let rid = Uuid::new_v4();
    let r = env
        .stream(&user_a(), &chat, json!({"content": "look", "request_id": rid, "attachment_ids": [doc, img]}))
        .await;
    assert_eq!(r.event_names().last().unwrap(), "done");
    let r = env.call(&user_a(), "POST", &format!("/mini-chat/v1/chats/{chat}/turns/{rid}/retry"), None).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let req = env.proxy.chat_requests().last().unwrap().json();
    assert!(req["input"].to_string().contains("input_image"), "image re-sent on retry");
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let atts = msgs["items"][0]["attachments"].as_array().unwrap().clone();
    assert_eq!(atts.len(), 2);
    // a soft-deleted attachment is silently excluded on the next edit
    let new_rid = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    env.sql_exec(&format!(
        "UPDATE attachments SET deleted_at = '2026-01-01T00:00:00.000001Z' WHERE id = x'{}'",
        img.replace('-', "")
    ))
    .await;
    let r = env
        .call(&user_a(), "PATCH", &format!("/mini-chat/v1/chats/{chat}/turns/{new_rid}"), Some(json!({"content": "edited"})))
        .await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let atts = msgs["items"][0]["attachments"].as_array().unwrap().clone();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0]["attachment_id"], doc.as_str());
    let req = env.proxy.chat_requests().last().unwrap().json();
    assert!(!req["input"].to_string().contains("input_image"));
}

#[tokio::test]
async fn code_interpreter_daily_quota_is_enforced() {
    let env = TestEnv::with(EnvOptions {
        config: json!({"quota": {"code_interpreter_daily_quota": 1}}),
        ..Default::default()
    })
    .await;
    let chat = env.chat(None).await;
    let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let att = id_of(&upload(&env, &chat, "t.xlsx", xlsx, b"PK\x03\x04data".to_vec()).await);
    let r = env.stream(&user_a(), &chat, json!({"content": "analyze", "attachment_ids": [att]})).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    env.sql_exec("UPDATE quota_usage SET code_interpreter_calls = 1 WHERE bucket = 'total' AND period_type = 'daily'").await;
    let r = env.send_msg(&chat, "more").await;
    r.assert_problem(429, "quota_exceeded");
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "code_interpreter");
}
