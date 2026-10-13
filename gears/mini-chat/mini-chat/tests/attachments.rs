//! US5 — attachments: upload/get/delete, limits and validation, indexing, provider tool wiring
//! (T058, T059).

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

#[allow(clippy::unwrap_used, reason = "test helper: an unreadable response must fail the test")]
async fn upload(env: &TestEnv, who: &str, chat: Uuid, name: &str, ct: Option<&str>, data: &[u8]) -> (StatusCode, Value) {
    let (content_type, body) = multipart(name, ct, data);
    let r = env
        .raw(
            who,
            Request::builder()
                .method(Method::POST)
                .uri(format!("/mini-chat/v1/chats/{chat}/attachments"))
                .header("content-type", content_type),
            Body::from(body),
        )
        .await;
    let status = r.status();
    let retry_after = r.headers().get("retry-after").map(|v| v.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
    let mut v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if let Some(ra) = retry_after {
        v["_retry_after"] = json!(ra);
    }
    (status, v)
}

#[tokio::test]
async fn document_upload_indexes_into_the_chat_vector_store() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let (s, v) = upload(&env, "a1", chat, "notes.txt", Some("text/plain"), b"hello world").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "ready");
    assert_eq!(v["kind"], "document");
    assert_eq!(v["filename"], "notes.txt");
    assert_eq!(v["content_type"], "text/plain");
    assert_eq!(v["size_bytes"], 11);
    assert!(v.get("img_thumbnail").is_none());
    assert!(!v.to_string().contains("file-"), "provider id leaked: {v}");
    let paths: Vec<String> = env.mock.requests().iter().map(|r| format!("{} {}", r.method, r.path)).collect();
    assert!(paths.iter().any(|p| p == "POST /v1/files"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "POST /v1/vector_stores"), "{paths:?}");
    assert!(paths.iter().any(|p| p.starts_with("POST /v1/vector_stores/") && p.ends_with("/files")), "{paths:?}");
    let add = env.mock.requests().into_iter().find(|r| r.path.ends_with("/files") && r.path.contains("vector_stores")).unwrap();
    assert_eq!(add.body["attributes"]["attachment_id"], v["id"]);
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_vector_stores WHERE vector_store_id IS NOT NULL").await, 1);

    // A second document reuses the same vector store.
    let (s, _) = upload(&env, "a1", chat, "more.md", Some("text/markdown"), b"# more").await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(env.mock.requests().iter().filter(|r| r.path == "/v1/vector_stores").count(), 1);

    // GET returns the same metadata; other users get 404.
    let id = v["id"].as_str().unwrap();
    let (s, g) = env.json("a1", Method::GET, &format!("/chats/{chat}/attachments/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(g["status"], "ready");
    let (s, g) = env.json("a2", Method::GET, &format!("/chats/{chat}/attachments/{id}"), None).await;
    assert_problem(s, &g, 404, "not_found");
}

#[tokio::test]
async fn image_upload_generates_a_thumbnail_and_is_not_indexed() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let (s, v) = upload(&env, "a1", chat, "pic.png", Some("application/octet-stream"), &png(400, 200)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["kind"], "image");
    assert_eq!(v["content_type"], "image/png");
    assert_eq!(v["status"], "ready");
    let t = &v["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp");
    assert_eq!(t["width"], 128);
    assert_eq!(t["height"], 64);
    assert!(!t["data_base64"].as_str().unwrap().is_empty());
    assert!(env.mock.requests().iter().all(|r| !r.path.contains("vector_stores")));
}

#[tokio::test]
async fn upload_validation_errors() {
    let env = TestEnv::with(EnvOptions { config: json!({"rag": {"uploaded_file_max_size_kb": 1}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    let (s, v) = upload(&env, "a1", chat, "a.mp4", Some("video/mp4"), b"x").await;
    assert_problem(s, &v, 400, "invalid_argument");
    assert_eq!(violation_reason(&v).as_deref(), Some("UNSUPPORTED_CONTENT_TYPE"));
    let (s, v) = upload(&env, "a1", chat, "blob.bin", Some("application/octet-stream"), b"x").await;
    assert_eq!(violation_reason(&v).as_deref(), Some("UNSUPPORTED_CONTENT_TYPE"), "{s} {v}");
    let (s, v) = upload(&env, "a1", chat, "a.txt", None, b"x").await;
    assert_problem(s, &v, 400, "invalid_argument");
    assert_eq!(violation_reason(&v).as_deref(), Some("MISSING_CONTENT_TYPE"));
    let (s, v) = upload(&env, "a1", chat, "big.txt", Some("text/plain"), &vec![b'a'; 2048]).await;
    assert_problem(s, &v, 400, "out_of_range");
    assert_eq!(violation_reason(&v).as_deref(), Some("FILE_TOO_LARGE"));

    // No boundary / no file field.
    let r = env
        .raw(
            "a1",
            Request::builder().method(Method::POST).uri(format!("/mini-chat/v1/chats/{chat}/attachments")).header("content-type", "multipart/form-data"),
            Body::from("x"),
        )
        .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(violation_reason(&v).as_deref(), Some("BOUNDARY_REQUIRED"));
    let body = "--B\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nx\r\n--B--\r\n";
    let r = env
        .raw(
            "a1",
            Request::builder().method(Method::POST).uri(format!("/mini-chat/v1/chats/{chat}/attachments")).header("content-type", "multipart/form-data; boundary=B"),
            Body::from(body),
        )
        .await;
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(violation_reason(&v).as_deref(), Some("MISSING_FILE"), "{v}");

    // Unknown chat → 404 before reading the body; nothing reached the provider.
    let (s, v) = upload(&env, "a1", Uuid::new_v4(), "a.txt", Some("text/plain"), b"x").await;
    assert_problem(s, &v, 404, "not_found");
    assert!(env.mock.requests().is_empty());
}

#[tokio::test]
async fn kill_switches_and_capabilities_gate_uploads() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    env.policy.kill_switches(|k| k.disable_images = true);
    let (s, v) = upload(&env, "a1", chat, "p.png", Some("image/png"), &png(10, 10)).await;
    assert_problem(s, &v, 400, "failed_precondition");
    assert_eq!(v["context"]["violations"][0]["subject"], "images");
    env.policy.kill_switches(|k| {
        k.disable_images = false;
        k.disable_code_interpreter = true;
    });
    let xlsx = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    let (s, v) = upload(&env, "a1", chat, "t.xlsx", Some(xlsx), b"PK..").await;
    assert_problem(s, &v, 400, "invalid_argument");
    env.policy.kill_switches(|k| k.disable_code_interpreter = false);
    let (s, v) = upload(&env, "a1", chat, "t.xlsx", Some(xlsx), b"PK..").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "ready");
    // XLSX is code-interpreter only: not added to a vector store.
    assert!(env.mock.requests().iter().all(|r| !r.path.contains("vector_stores")));
}

#[tokio::test]
async fn model_removed_from_catalog_rejects_upload_before_body() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({"model": "gpt-4.1-mini"})).await;
    env.policy.catalog(|c| c.retain(|m| m.id != "gpt-4.1-mini"));
    let (s, v) = upload(&env, "a1", chat, "a.txt", Some("text/plain"), b"x").await;
    assert_problem(s, &v, 400, "invalid_argument");
    assert_eq!(violation_reason(&v).as_deref(), Some("INVALID_MODEL"));
}

#[tokio::test]
async fn per_chat_limits() {
    let env = TestEnv::with(EnvOptions { config: json!({"rag": {"max_documents_per_chat": 1, "max_total_upload_mb_per_chat": 1}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    let (s, _) = upload(&env, "a1", chat, "a.txt", Some("text/plain"), b"x").await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, v) = upload(&env, "a1", chat, "b.txt", Some("text/plain"), b"y").await;
    assert_problem(s, &v, 429, "resource_exhausted");
    assert!(v.to_string().contains("document_limit"), "{v}");
    let (s, v) = upload(&env, "a1", chat, "big.png", Some("image/png"), &vec![0u8; 1_100_000]).await;
    assert_problem(s, &v, 429, "resource_exhausted");
    assert!(v.to_string().contains("storage_limit"), "{v}");
}

#[tokio::test]
async fn provider_failures_mark_the_row_failed_and_return_503() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    env.mock.configure(|c| c.file_upload_status = 500);
    let (s, v) = upload(&env, "a1", chat, "a.txt", Some("text/plain"), b"x").await;
    assert_problem(s, &v, 503, "service_unavailable");
    assert_eq!(v["_retry_after"], "10");
    assert_eq!(env.count("SELECT COUNT(*) FROM attachments WHERE status = 'failed' AND error_code = 'upload_failed'").await, 1);
    env.mock.configure(|c| {
        c.file_upload_status = 200;
        c.index_status = "failed".into();
    });
    let (s, v) = upload(&env, "a1", chat, "b.txt", Some("text/plain"), b"x").await;
    assert_problem(s, &v, 503, "service_unavailable");
    assert!(!v.to_string().contains("indexing_failed"));
    assert_eq!(env.count("SELECT COUNT(*) FROM attachments WHERE status = 'failed' AND error_code = 'indexing_failed'").await, 1);
    // Best-effort delete of the provider file.
    let deleted = env
        .eventually(Duration::from_secs(5), || async {
            env.mock.requests().iter().any(|r| r.method == Method::DELETE && r.path.starts_with("/v1/files/")).then_some(())
        })
        .await;
    assert!(deleted.is_some());
    // Failed rows stay visible with their error code.
    let id = env.query("SELECT hex(id) AS h FROM attachments WHERE error_code = 'indexing_failed'").await;
    let hex: String = id[0].try_get("", "h").unwrap();
    let uid = Uuid::parse_str(&hex).unwrap();
    let (s, g) = env.json("a1", Method::GET, &format!("/chats/{chat}/attachments/{uid}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(g["status"], "failed");
    assert_eq!(g["error_code"], "indexing_failed");
}

#[tokio::test]
async fn ready_attachments_are_wired_into_provider_tools() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let (_, doc) = upload(&env, "a1", chat, "doc.txt", Some("text/plain"), b"contract text").await;
    let (_, img) = upload(&env, "a1", chat, "p.png", Some("image/png"), &png(20, 20)).await;
    let (_, xls) = upload(&env, "a1", chat, "t.xlsx", Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"), b"PK").await;
    let r = env.stream("a1", chat, json!({"content": "summarize [[file_search]] [[code]]", "attachment_ids": [img["id"], doc["id"]]})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.error);
    let req = env.mock.responses_requests().last().unwrap().body.clone();
    let tools = req["tools"].as_array().unwrap();
    let fs = tools.iter().find(|t| t["type"] == "file_search").expect("file_search tool");
    assert_eq!(fs["vector_store_ids"].as_array().unwrap().len(), 1);
    assert_eq!(fs["max_num_results"], 5);
    let ci = tools.iter().find(|t| t["type"] == "code_interpreter").expect("code_interpreter tool");
    assert_eq!(ci["container"]["file_ids"].as_array().unwrap().len(), 1);
    assert_eq!(req["max_tool_calls"], 2);
    let last = req["input"].as_array().unwrap().last().unwrap().clone();
    assert!(last["content"].as_array().unwrap().iter().any(|p| p["type"] == "input_image"));
    // Citations map provider file ids to attachment ids and filenames.
    let cites = r.event("citations").expect("citations");
    assert_eq!(cites["items"][0]["source"], "file");
    assert_eq!(cites["items"][0]["attachment_id"], doc["id"]);
    assert_eq!(cites["items"][0]["title"], "doc.txt");
    // code interpreter tool event carries output.
    assert!(r.events.iter().any(|(n, v)| n == "tool" && v["name"] == "code_interpreter" && v["phase"] == "done"));
    // Message attachments are listed on the user message.
    let (_, msgs) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    let atts = msgs["items"][0]["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 2);
    assert!(atts.iter().any(|a| a["kind"] == "image" && a["img_thumbnail"]["content_type"] == "image/webp"));
    let _ = xls;
}

#[tokio::test]
async fn vision_is_checked_against_the_effective_model() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({"model": "text-only"})).await;
    let (_, img) = upload(&env, "a1", chat, "p.png", Some("image/png"), &png(20, 20)).await;
    let r = env.stream("a1", chat, json!({"content": "what is this", "attachment_ids": [img["id"]]})).await;
    assert_problem(r.status, &r.error, 400, "invalid_argument");
    assert_eq!(violation_reason(&r.error).as_deref(), Some("VISION_NOT_SUPPORTED"));
}

#[tokio::test]
async fn too_many_images_is_rejected() {
    let env = TestEnv::with(EnvOptions { config: json!({"rag": {"max_images_per_message": 1}}), ..EnvOptions::default() }).await;
    let chat = env.create_chat("a1", json!({})).await;
    let (_, a) = upload(&env, "a1", chat, "a.png", Some("image/png"), &png(10, 10)).await;
    let (_, b) = upload(&env, "a1", chat, "b.png", Some("image/png"), &png(10, 10)).await;
    let r = env.stream("a1", chat, json!({"content": "x", "attachment_ids": [a["id"], b["id"]]})).await;
    assert_problem(r.status, &r.error, 400, "out_of_range");
    assert_eq!(violation_reason(&r.error).as_deref(), Some("TOO_MANY_IMAGES"));
}

#[tokio::test]
async fn delete_attachment_lifecycle() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    let (_, used) = upload(&env, "a1", chat, "used.txt", Some("text/plain"), b"u").await;
    let (_, free) = upload(&env, "a1", chat, "free.txt", Some("text/plain"), b"f").await;
    env.stream("a1", chat, json!({"content": "hi", "attachment_ids": [used["id"]]})).await;
    let used_id = used["id"].as_str().unwrap();
    let free_id = free["id"].as_str().unwrap();
    let (s, v) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/attachments/{used_id}"), None).await;
    assert_problem(s, &v, 409, "already_exists");
    assert_eq!(v["context"]["resource_name"], "attachment_locked");
    let (s, _) = env.json("a2", Method::DELETE, &format!("/chats/{chat}/attachments/{free_id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/attachments/{free_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{chat}/attachments/{free_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = env.json("a1", Method::GET, &format!("/chats/{chat}/attachments/{free_id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // The outbox handler deletes the provider file and marks cleanup done (one event only).
    let done = env
        .eventually(Duration::from_secs(10), || async {
            (env.count("SELECT COUNT(*) FROM attachments WHERE cleanup_status = 'done'").await == 1).then_some(())
        })
        .await;
    assert!(done.is_some());
    assert_eq!(env.mock.requests().iter().filter(|r| r.method == Method::DELETE).count(), 1);
}
