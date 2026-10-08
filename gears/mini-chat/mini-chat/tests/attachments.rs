//! Attachments: upload, validation, indexing failures, tool wiring, citations,
//! deletion, chat-deletion cleanup and the upload reaper (acceptance criteria:
//! Attachments; DESIGN §3.3, §3.6 "File Upload", §4 "Attachment Deletion",
//! "Cleanup on Chat Deletion", Appendix B.9.5).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const CHAT_RT: &str = "gts.cf.core.mini_chat.chat.v1~";

// ── local helpers ────────────────────────────────────────────────────────

/// Provider request URIs (path part) of the given method.
fn uris(h: &Harness, method: &str) -> Vec<String> {
    h.provider
        .requests
        .lock()
        .iter()
        .filter(|r| r.method == method)
        .map(|r| r.uri.split('?').next().unwrap_or_default().to_owned())
        .collect()
}

fn is_file_upload(u: &str) -> bool {
    u.ends_with("/files") && !u.contains("/vector_stores")
}

fn is_vs_create(u: &str) -> bool {
    u.ends_with("/vector_stores")
}

fn is_vs_add(u: &str) -> bool {
    u.contains("/vector_stores/") && u.ends_with("/files")
}

fn id_of(r: &HttpResp) -> Uuid {
    Uuid::parse_str(r.json()["id"].as_str().unwrap_or_else(|| panic!("no id: {}", r.text))).unwrap()
}

async fn upload_text(h: &Harness, u: &toolkit_security::SecurityContext, chat: Uuid, name: &str) -> Uuid {
    let r = h.upload(u, chat, name, "text/plain", b"hello attachment content").await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.json()["status"], "ready", "{}", r.text);
    id_of(&r)
}

async fn att_col(h: &Harness, id: Uuid, col: &str) -> Option<String> {
    h.scalar_str(&format!("SELECT {col} FROM attachments WHERE hex(id) = '{}'", hex_uuid(id)))
        .await
}

async fn att_i64(h: &Harness, id: Uuid, col: &str) -> i64 {
    h.scalar_i64(&format!("SELECT {col} FROM attachments WHERE hex(id) = '{}'", hex_uuid(id)))
        .await
}

async fn provider_file_id(h: &Harness, id: Uuid) -> String {
    att_col(h, id, "provider_file_id").await.expect("provider_file_id")
}

async fn wait_cleanup(h: &Harness, id: Uuid, want: &str) {
    h.eventually(|| async { att_col(h, id, "cleanup_status").await.as_deref() == Some(want) })
        .await;
}

async fn wait_provider_delete(h: &Harness, needle: &str) {
    let needle = needle.to_owned();
    for _ in 0..200 {
        if h.provider.count("DELETE", &needle) > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("provider never received DELETE of {needle}");
}

/// `DELETE /chats/{id}`.
async fn delete_chat(h: &Harness, u: &toolkit_security::SecurityContext, chat: Uuid) -> HttpResp {
    h.call(u, "DELETE", &format!("/mini-chat/v1/chats/{chat}"), None).await
}

fn att_uri(chat: Uuid, id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/attachments/{id}")
}

fn last_chat_request(h: &Harness) -> Value {
    h.provider.chat_requests().last().cloned().expect("a provider chat request")
}

fn assert_no_provider_ids(text: &str) {
    for needle in ["provider_file_id", "file-", "vs_", "vector_store"] {
        assert!(!text.contains(needle), "response leaks `{needle}`: {text}");
    }
}

// ── 1/2: successful uploads ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn upload_text_document_is_indexed_and_hides_provider_ids() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let r = h.upload(&u, chat, "notes.txt", "text/plain", b"some plain text").await;
    assert_eq!(r.status, 201, "{}", r.text);
    let body = r.json();
    assert_eq!(body["kind"], "document");
    assert_eq!(body["status"], "ready");
    assert_eq!(body["filename"], "notes.txt");
    assert_eq!(body["content_type"], "text/plain");
    assert_eq!(body["size_bytes"], 15);
    assert!(body.get("doc_summary").is_none(), "doc_summary must be absent");
    assert!(body.get("error_code").is_none());
    assert!(body.get("img_thumbnail").is_none());
    assert_no_provider_ids(&r.text);
    let id = id_of(&r);

    let g = h.call(&u, "GET", &att_uri(chat, id), None).await;
    assert_eq!(g.status, 200, "{}", g.text);
    assert_eq!(g.json(), body, "GET returns the same detail");
    assert_no_provider_ids(&g.text);

    let vs_rows = h
        .scalar_i64(&format!(
            "SELECT count(*) FROM chat_vector_stores WHERE hex(chat_id) = '{}' AND vector_store_id IS NOT NULL",
            hex_uuid(chat)
        ))
        .await;
    assert_eq!(vs_rows, 1);

    let posts = uris(&h, "POST");
    assert_eq!(posts.iter().filter(|u| is_file_upload(u)).count(), 1, "{posts:?}");
    assert_eq!(posts.iter().filter(|u| is_vs_create(u)).count(), 1, "{posts:?}");
    assert_eq!(posts.iter().filter(|u| is_vs_add(u)).count(), 1, "{posts:?}");
    // vector store add references the uploaded file id
    let fid = provider_file_id(&h, id).await;
    let add = h
        .provider
        .requests
        .lock()
        .iter()
        .find(|r| r.method == "POST" && is_vs_add(r.uri.split('?').next().unwrap()))
        .cloned()
        .unwrap();
    assert!(add.body.to_string().contains(&fid), "{:?}", add.body);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_png_image_gets_thumbnail_and_no_vector_store() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let r = h.upload(&u, chat, "pic.png", "image/png", &png(300, 200)).await;
    assert_eq!(r.status, 201, "{}", r.text);
    let body = r.json();
    assert_eq!(body["kind"], "image");
    assert_eq!(body["status"], "ready");
    let t = &body["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp", "{body}");
    let (w, hh) = (t["width"].as_i64().unwrap(), t["height"].as_i64().unwrap());
    assert!(w > 0 && w <= 128 && hh > 0 && hh <= 128, "{w}x{hh}");
    assert!(!t["data_base64"].as_str().unwrap().is_empty());
    assert_no_provider_ids(&r.text);
    let g = h.call(&u, "GET", &att_uri(chat, id_of(&r)), None).await;
    assert_eq!(g.json(), body);
    assert!(
        h.provider.requests.lock().iter().all(|r| !r.uri.contains("vector_stores")),
        "images never touch the vector store"
    );
    assert_eq!(uris(&h, "POST").iter().filter(|u| is_file_upload(u)).count(), 1);
    h.shutdown().await;
}

// ── 3: validation ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn upload_validation_errors() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;

    // unsupported content type
    let r = h.upload(&u, chat, "a.zip", "application/zip", b"PK..").await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert_eq!(reason(&r.json()), "UNSUPPORTED_CONTENT_TYPE");

    // octet-stream with a .pdf filename is inferred as application/pdf
    let r = h.upload(&u, chat, "report.pdf", "application/octet-stream", b"%PDF-1.4 fake").await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.json()["content_type"], "application/pdf");
    assert_eq!(r.json()["kind"], "document");

    // octet-stream with an unknown extension stays unsupported
    let r = h.upload(&u, chat, "blob.bin", "application/octet-stream", b"xx").await;
    assert_eq!((r.status, reason(&r.json())), (400, "UNSUPPORTED_CONTENT_TYPE".to_owned()), "{}", r.text);

    // missing multipart boundary
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", "multipart/form-data")
        .body(axum::body::Body::from("whatever"))
        .unwrap();
    let r = h.raw(&u, req).await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert_eq!(reason(&r.json()), "BOUNDARY_REQUIRED");

    // multipart without a `file` field
    let body = "--B\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nvalue\r\n--B--\r\n";
    let req = http::Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
        .header("content-type", "multipart/form-data; boundary=B")
        .body(axum::body::Body::from(body))
        .unwrap();
    let r = h.raw(&u, req).await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert_eq!(reason(&r.json()), "MISSING_FILE");

    // unknown chat → 404 chat resource
    let r = h.upload(&u, Uuid::new_v4(), "a.png", "image/png", &png(4, 4)).await;
    assert_eq!(r.status, 404, "{}", r.text);
    assert_eq!(r.json()["context"]["resource_type"], CHAT_RT);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_oversize_image_is_rejected() {
    let mut cfg = base_config();
    cfg.rag.uploaded_image_max_size_kb = 1;
    let h = Harness::with(cfg, policy_cfg(default_catalog())).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let r = h.upload(&u, chat, "big.png", "image/png", &vec![7u8; 4096]).await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert_eq!(r.json()["context"]["field_violations"][0]["reason"], "FILE_TOO_LARGE", "{}", r.text);
    // nothing was stored or sent to the provider
    assert_eq!(h.scalar_i64("SELECT count(*) FROM attachments").await, 0);
    assert!(uris(&h, "POST").iter().all(|u| !is_file_upload(u)));
    // a small image still fits
    let r = h.upload(&u, chat, "small.png", "image/png", &png(2, 2)).await;
    assert_eq!(r.status, 201, "{}", r.text);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_document_count_limit() {
    let mut cfg = base_config();
    cfg.rag.max_documents_per_chat = 1;
    let h = Harness::with(cfg, policy_cfg(default_catalog())).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    upload_text(&h, &u, chat, "one.txt").await;
    let r = h.upload(&u, chat, "two.txt", "text/plain", b"second").await;
    assert_eq!(r.status, 429, "{}", r.text);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "document_limit", "{}", r.text);
    // images do not count against the document limit
    let r = h.upload(&u, chat, "p.png", "image/png", &png(2, 2)).await;
    assert_eq!(r.status, 201, "{}", r.text);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_total_size_limit() {
    let mut cfg = base_config();
    cfg.rag.max_total_upload_mb_per_chat = 1;
    let h = Harness::with(cfg, policy_cfg(default_catalog())).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let half = vec![b'a'; 600 * 1024];
    let r = h.upload(&u, chat, "a.txt", "text/plain", &half).await;
    assert_eq!(r.status, 201, "{}", r.text);
    let r = h.upload(&u, chat, "b.txt", "text/plain", &half).await;
    assert_eq!(r.status, 429, "{}", r.text);
    assert_eq!(r.json()["context"]["violations"][0]["subject"], "storage_limit", "{}", r.text);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_xlsx_without_code_interpreter_is_rejected() {
    let mut catalog = default_catalog();
    let mut noci = model("noci", "standard", false, true);
    noci["general_config"]["tool_support"]["code_interpreter"] = json!(false);
    catalog.push(noci);
    let h = Harness::with(base_config(), policy_cfg(catalog)).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "noci"})).await;
    let r = h.upload(&u, chat, "sheet.xlsx", XLSX, b"PK fake xlsx").await;
    assert_eq!(r.status, 400, "{}", r.text);
    assert_eq!(reason(&r.json()), "CODE_INTERPRETER_UNAVAILABLE", "{}", r.text);
    // inferred from the extension as well
    let r = h.upload(&u, chat, "sheet.xlsx", "application/octet-stream", b"PK fake xlsx").await;
    assert_eq!((r.status, reason(&r.json())), (400, "CODE_INTERPRETER_UNAVAILABLE".to_owned()), "{}", r.text);
    assert_eq!(h.scalar_i64("SELECT count(*) FROM attachments").await, 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_image_with_disable_images_kill_switch() {
    let mut pc = policy_cfg(default_catalog());
    pc.kill_switches.disable_images = true;
    let h = Harness::with(base_config(), pc).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let r = h.upload(&u, chat, "p.png", "image/png", &png(4, 4)).await;
    assert_eq!(r.status, 400, "{}", r.text);
    let v = r.json();
    assert_eq!(v["context"]["violations"][0]["subject"], "images", "{v}");
    assert_eq!(v["context"]["violations"][0]["type"], "FEATURE_DISABLED", "{v}");
    let text = r.text.to_lowercase();
    assert!(text.contains("failed_precondition") || text.contains("failed-precondition") || text.contains("failedprecondition"), "{}", r.text);
    // documents are unaffected
    upload_text(&h, &u, chat, "ok.txt").await;
    h.shutdown().await;
}

// ── 4/5: provider failures ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn indexing_failure_marks_row_failed_and_deletes_file() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    *h.provider.vs_status.lock() = "failed".into();
    let r = h.upload(&u, chat, "doc.txt", "text/plain", b"will fail").await;
    assert_eq!(r.status, 503, "{}", r.text);
    assert_eq!(r.headers["retry-after"], "10");
    let id = Uuid::parse_str(
        &h.scalar_str(&format!(
            "SELECT lower(hex(id)) FROM attachments WHERE hex(chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await
        .unwrap(),
    )
    .unwrap();
    let g = h.call(&u, "GET", &att_uri(chat, id), None).await;
    assert_eq!(g.status, 200, "{}", g.text);
    assert_eq!(g.json()["status"], "failed");
    assert_eq!(g.json()["error_code"], "indexing_failed");
    assert_no_provider_ids(&g.text);
    let fid = provider_file_id(&h, id).await;
    wait_provider_delete(&h, &format!("/files/{fid}")).await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_upload_failure_marks_row_failed() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    *h.provider.upload_status.lock() = 500;
    let r = h.upload(&u, chat, "doc.txt", "text/plain", b"nope").await;
    assert_eq!(r.status, 503, "{}", r.text);
    assert_eq!(r.headers["retry-after"], "10");
    let rows = h
        .query(&format!(
            "SELECT status, error_code, provider_file_id FROM attachments WHERE hex(chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "failed");
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(1).unwrap().as_deref(), Some("upload_failed"));
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(2).unwrap(), None);
    // image uploads fail the same way
    let r = h.upload(&u, chat, "p.png", "image/png", &png(2, 2)).await;
    assert_eq!(r.status, 503, "{}", r.text);
    h.shutdown().await;
}

// ── 6: attachments available to tools ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn ready_document_enables_file_search() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    // no documents: no file_search tool
    assert_eq!(h.send(&u, chat, json!({"content": "before"})).await.status, 200);
    let before = last_chat_request(&h);
    let has_fs = |req: &Value| {
        req["tools"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["type"] == "file_search"))
    };
    assert!(!has_fs(&before), "{before}");

    upload_text(&h, &u, chat, "doc.txt").await;
    let vs = h
        .scalar_str(&format!(
            "SELECT vector_store_id FROM chat_vector_stores WHERE hex(chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await
        .unwrap();
    let r = h.send(&u, chat, json!({"content": "what does the doc say?"})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let req = last_chat_request(&h);
    let fs = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "file_search")
        .cloned()
        .unwrap_or_else(|| panic!("no file_search tool: {req}"));
    assert_eq!(fs["vector_store_ids"], json!([vs]));
    let instr = req["instructions"].as_str().unwrap();
    assert!(
        instr.contains(mini_chat::config::DEFAULT_FILE_SEARCH_GUARD),
        "file_search guard missing: {instr}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn xlsx_enables_code_interpreter_and_image_is_input_image() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let r = h.upload(&u, chat, "sheet.xlsx", XLSX, b"PK fake xlsx").await;
    assert_eq!(r.status, 201, "{}", r.text);
    assert_eq!(r.json()["status"], "ready");
    assert_eq!(r.json()["kind"], "document");
    let xid = id_of(&r);
    let xfid = provider_file_id(&h, xid).await;
    assert!(
        h.provider.requests.lock().iter().all(|r| !r.uri.contains("vector_stores")),
        "XLSX is code-interpreter only (no vector store)"
    );
    let img = h.upload(&u, chat, "p.png", "image/png", &png(8, 8)).await;
    assert_eq!(img.status, 201, "{}", img.text);
    let iid = id_of(&img);
    let ifid = provider_file_id(&h, iid).await;

    let r = h
        .send(&u, chat, json!({"content": "analyze", "attachment_ids": [iid]}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.sse().last().unwrap().0, "done", "{}", r.text);
    let req = last_chat_request(&h);
    let ci = req["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "code_interpreter")
        .cloned()
        .unwrap_or_else(|| panic!("no code_interpreter tool: {req}"));
    assert_eq!(ci["container"]["file_ids"], json!([xfid]));
    assert_eq!(req["include"], json!(["code_interpreter_call.outputs"]));
    let last_input = req["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last_input["role"], "user");
    let parts = last_input["content"].as_array().unwrap_or_else(|| panic!("{last_input}"));
    assert!(parts.iter().any(|p| p["type"] == "input_text" && p["text"] == "analyze"), "{last_input}");
    assert!(
        parts.iter().any(|p| p["type"] == "input_image" && p["file_id"] == ifid.as_str()),
        "{last_input}"
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_attachment_ids_are_rejected_before_the_turn() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let ready = upload_text(&h, &u, chat, "ok.txt").await;
    // a failed (non-ready) attachment
    *h.provider.upload_status.lock() = 500;
    assert_eq!(h.upload(&u, chat, "bad.png", "image/png", &png(2, 2)).await.status, 503);
    *h.provider.upload_status.lock() = 200;
    let failed = Uuid::parse_str(
        &h.scalar_str(&format!(
            "SELECT lower(hex(id)) FROM attachments WHERE hex(chat_id) = '{}' AND status = 'failed'",
            hex_uuid(chat)
        ))
        .await
        .unwrap(),
    )
    .unwrap();
    // an attachment of another chat of the same user
    let other_chat = h.create_chat(&u, json!({"model": "std"})).await;
    let foreign = upload_text(&h, &u, other_chat, "other.txt").await;

    let chat_calls = h.provider.chat_requests().len();
    for ids in [
        json!([failed]),
        json!([Uuid::new_v4()]),
        json!([ready, ready]),
        json!([foreign]),
    ] {
        let r = h.send(&u, chat, json!({"content": "hi", "attachment_ids": ids})).await;
        assert_eq!(r.status, 400, "{ids}: {}", r.text);
        assert_eq!(r.json()["context"]["field_violations"][0]["reason"], "invalid_attachment", "{ids}: {}", r.text);
    }
    assert_eq!(h.provider.chat_requests().len(), chat_calls, "no provider chat call");
    let turns = h
        .scalar_i64(&format!("SELECT count(*) FROM chat_turns WHERE hex(chat_id) = '{}'", hex_uuid(chat)))
        .await;
    assert_eq!(turns, 0, "no chat_turns row");
    let msgs = h
        .scalar_i64(&format!("SELECT count(*) FROM messages WHERE hex(chat_id) = '{}'", hex_uuid(chat)))
        .await;
    assert_eq!(msgs, 0);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn file_citations_map_to_attachment() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let id = upload_text(&h, &u, chat, "Quarterly Report.txt").await;
    let fid = provider_file_id(&h, id).await;
    h.provider.push(Reply::Events(vec![
        delta("According to the report"),
        ev(
            "response.output_text.annotation.added",
            json!({"type": "response.output_text.annotation.added",
                   "annotation": {"type": "file_citation", "file_id": fid, "filename": "x"}}),
        ),
        // an unknown file id is not surfaced
        ev(
            "response.output_text.annotation.added",
            json!({"type": "response.output_text.annotation.added",
                   "annotation": {"type": "file_citation", "file_id": "file-unknown", "filename": "y"}}),
        ),
        completed(100, 50),
    ]));
    let r = h.send(&u, chat, json!({"content": "summarize the report"})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let events = r.sse();
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    let pos = names.iter().position(|n| *n == "citations").unwrap_or_else(|| panic!("{names:?}"));
    assert_eq!(names.last(), Some(&"done"), "{names:?}");
    assert!(pos < names.len() - 1, "citations before done");
    let items = events[pos].1["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1, "{items:?}");
    let c = &items[0];
    assert_eq!(c["source"], "file");
    assert_eq!(c["attachment_id"], id.to_string());
    assert_eq!(c["title"], "Quarterly Report.txt");
    assert_eq!(c["snippet"], "");
    assert!(!r.text.contains(&fid), "provider file id must not leak into SSE");
    h.shutdown().await;
}

// ── 7: attachment deletion ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn delete_unreferenced_attachment_cleans_up_provider_file() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let id = upload_text(&h, &u, chat, "doc.txt").await;
    let fid = provider_file_id(&h, id).await;
    let d = h.call(&u, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(d.status, 204, "{}", d.text);
    let g = h.call(&u, "GET", &att_uri(chat, id), None).await;
    assert_eq!(g.status, 404, "{}", g.text);
    assert_eq!(g.json()["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
    let d2 = h.call(&u, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(d2.status, 204, "repeated DELETE is idempotent: {}", d2.text);
    wait_provider_delete(&h, &format!("/files/{fid}")).await;
    wait_cleanup(&h, id, "done").await;
    assert!(att_col(&h, id, "deleted_at").await.is_some());
    // unknown attachment → 404
    let r = h.call(&u, "DELETE", &att_uri(chat, Uuid::new_v4()), None).await;
    assert_eq!(r.status, 404);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_referenced_attachment_is_locked() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let img = h.upload(&u, chat, "p.png", "image/png", &png(4, 4)).await;
    assert_eq!(img.status, 201);
    let id = id_of(&img);
    let r = h.send(&u, chat, json!({"content": "look", "attachment_ids": [id]})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let d = h.call(&u, "DELETE", &att_uri(chat, id), None).await;
    assert_eq!(d.status, 409, "{}", d.text);
    assert_eq!(d.json()["context"]["resource_name"], "attachment_locked", "{}", d.text);
    let g = h.call(&u, "GET", &att_uri(chat, id), None).await;
    assert_eq!(g.status, 200, "still visible");
    assert_eq!(h.provider.count("DELETE", "/files/"), 0);
    h.shutdown().await;
}

// ── 8: chat deletion cleanup ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn chat_deletion_cleans_up_files_and_vector_store() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let doc = upload_text(&h, &u, chat, "doc.txt").await;
    let img = h.upload(&u, chat, "p.png", "image/png", &png(4, 4)).await;
    let img = id_of(&img);
    let dfid = provider_file_id(&h, doc).await;
    let ifid = provider_file_id(&h, img).await;
    let vs = h
        .scalar_str(&format!(
            "SELECT vector_store_id FROM chat_vector_stores WHERE hex(chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await
        .unwrap();
    let d = delete_chat(&h, &u, chat).await;
    assert_eq!(d.status, 204, "{}", d.text);
    wait_provider_delete(&h, &format!("/files/{dfid}")).await;
    wait_provider_delete(&h, &format!("/files/{ifid}")).await;
    wait_provider_delete(&h, &format!("/vector_stores/{vs}")).await;
    wait_cleanup(&h, doc, "done").await;
    wait_cleanup(&h, img, "done").await;
    h.eventually(|| async {
        h.scalar_i64(&format!(
            "SELECT count(*) FROM chat_vector_stores WHERE hex(chat_id) = '{}'",
            hex_uuid(chat)
        ))
        .await
            == 0
    })
    .await;
    // files are deleted before the vector store
    let reqs = h.provider.requests.lock().clone();
    let pos = |needle: &str| reqs.iter().position(|r| r.method == "DELETE" && r.uri.contains(needle)).unwrap();
    assert!(pos(&format!("/files/{dfid}")) < pos(&format!("/vector_stores/{vs}")));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_deletion_cleanup_gives_up_after_max_attempts() {
    let mut cfg = base_config();
    cfg.cleanup_worker.max_attempts = 2;
    let h = Harness::with(cfg, policy_cfg(default_catalog())).await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let doc = upload_text(&h, &u, chat, "doc.txt").await;
    let fid = provider_file_id(&h, doc).await;
    *h.provider.delete_status.lock() = 500;
    let d = delete_chat(&h, &u, chat).await;
    assert_eq!(d.status, 204, "{}", d.text);
    h.eventually(|| async { att_i64(&h, doc, "cleanup_attempts").await >= 1 }).await;
    wait_cleanup(&h, doc, "failed").await;
    assert_eq!(att_i64(&h, doc, "cleanup_attempts").await, 2);
    assert!(att_col(&h, doc, "last_cleanup_error").await.is_some());
    assert_eq!(h.provider.count("DELETE", &format!("/files/{fid}")), 2);
    // no further attempts after the terminal state
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert_eq!(h.provider.count("DELETE", &format!("/files/{fid}")), 2);
    assert_eq!(att_i64(&h, doc, "cleanup_attempts").await, 2);
    h.shutdown().await;
}

// ── 9: upload reaper ─────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn upload_reaper_fails_abandoned_rows() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({"model": "std"})).await;
    let pending = id_of(&h.upload(&u, chat, "a.png", "image/png", &png(2, 2)).await);
    let uploaded = id_of(&h.upload(&u, chat, "b.png", "image/png", &png(2, 2)).await);
    let fresh = id_of(&h.upload(&u, chat, "c.png", "image/png", &png(2, 2)).await);
    let ufid = provider_file_id(&h, uploaded).await;
    h.exec(&format!(
        "UPDATE attachments SET status = 'pending', provider_file_id = NULL, updated_at = '2000-01-01 00:00:00+00:00' WHERE hex(id) = '{}'",
        hex_uuid(pending)
    ))
    .await;
    h.exec(&format!(
        "UPDATE attachments SET status = 'uploaded', updated_at = '2000-01-01 00:00:00+00:00' WHERE hex(id) = '{}'",
        hex_uuid(uploaded)
    ))
    .await;
    // a recently updated in-flight row is left alone
    h.exec(&format!(
        "UPDATE attachments SET status = 'uploaded' WHERE hex(id) = '{}'",
        hex_uuid(fresh)
    ))
    .await;

    let n = mini_chat::infra::workers::upload_reaper_scan(&h.svc).await.unwrap();
    assert_eq!(n, 2);

    assert_eq!(att_col(&h, pending, "status").await.as_deref(), Some("failed"));
    assert_eq!(att_col(&h, pending, "error_code").await.as_deref(), Some("upload_abandoned"));
    assert_eq!(att_col(&h, pending, "cleanup_status").await, None, "no provider file to delete");
    assert!(att_col(&h, pending, "deleted_at").await.is_none(), "row is not soft-deleted");
    let g = h.call(&u, "GET", &att_uri(chat, pending), None).await;
    assert_eq!((g.json()["status"].as_str(), g.json()["error_code"].as_str()), (Some("failed"), Some("upload_abandoned")));

    assert_eq!(att_col(&h, uploaded, "status").await.as_deref(), Some("failed"));
    assert_eq!(att_col(&h, uploaded, "error_code").await.as_deref(), Some("upload_abandoned"));
    let cs = att_col(&h, uploaded, "cleanup_status").await;
    assert!(matches!(cs.as_deref(), Some("pending" | "done")), "{cs:?}");
    wait_provider_delete(&h, &format!("/files/{ufid}")).await;
    wait_cleanup(&h, uploaded, "done").await;

    assert_eq!(att_col(&h, fresh, "status").await.as_deref(), Some("uploaded"));
    // a second scan finds nothing
    assert_eq!(mini_chat::infra::workers::upload_reaper_scan(&h.svc).await.unwrap(), 0);
    h.shutdown().await;
}

