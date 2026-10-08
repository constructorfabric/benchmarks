#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use http::{Request, StatusCode};
use toolkit::api::OpenApiRegistryImpl;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_routes;
use crate::domain::service::attachments::test_helpers::{insert_chat, multipart_body, png};
use crate::domain::service::test_support::{TENANT_A, TestEnv, TestOptions, USER_A1, ctx_a1};

const BOUNDARY: &str = "XyZboundary123";

fn router(env: &TestEnv) -> Router {
    let openapi = OpenApiRegistryImpl::new();
    register_routes(Router::new(), &openapi, Arc::clone(&env.services), "/mini-chat")
}

fn upload_request(chat: Uuid, content_type: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/attachments"));
    if let Some(ct) = content_type {
        b = b.header("content-type", ct);
    }
    let mut req = b.body(Body::from(body)).unwrap();
    req.extensions_mut().insert(ctx_a1());
    req
}

fn plain_request(method: &str, uri: &str) -> Request<Body> {
    let mut req = Request::builder().method(method).uri(uri).body(Body::empty()).unwrap();
    req.extensions_mut().insert(ctx_a1());
    req
}

fn multipart_ct() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

async fn send(router: &Router, req: Request<Body>) -> (StatusCode, http::HeaderMap, serde_json::Value) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, headers, json)
}

fn file_part<'a>(filename: Option<&'a str>, ct: Option<&'a str>, data: &'a [u8]) -> (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]) {
    ("file", filename, ct, data)
}

#[tokio::test]
async fn upload_get_delete_roundtrip() {
    let env = TestEnv::default_env().await;
    let r = router(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let body = multipart_body(
        BOUNDARY,
        &[
            ("note", None, None, b"ignored field"),
            file_part(Some("report.pdf"), Some("application/pdf"), b"%PDF-1.4 data"),
        ],
    );
    let (status, _, json) = send(&r, upload_request(chat, Some(&multipart_ct()), body)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "ready");
    assert_eq!(json["kind"], "document");
    assert_eq!(json["filename"], "report.pdf");
    assert_eq!(json["content_type"], "application/pdf");
    assert_eq!(json["size_bytes"], 13);
    for absent in ["error_code", "doc_summary", "img_thumbnail", "summary_updated_at", "provider_file_id"] {
        assert!(json.get(absent).is_none(), "{absent} must be omitted: {json}");
    }
    let id = json["id"].as_str().unwrap().to_owned();

    let uri = format!("/mini-chat/v1/chats/{chat}/attachments/{id}");
    let (status, _, got) = send(&r, plain_request("GET", &uri)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], json["id"]);

    let (status, _, _) = send(&r, plain_request("DELETE", &uri)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(&r, plain_request("DELETE", &uri)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, err) = send(&r, plain_request("GET", &uri)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");

    let (status, _, err) = send(
        &r,
        plain_request("GET", &format!("/mini-chat/v1/chats/{chat}/attachments/not-a-uuid")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
    env.shutdown().await;
}

#[tokio::test]
async fn image_upload_returns_thumbnail_and_default_filename() {
    let env = TestEnv::default_env().await;
    let r = router(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let img = png(64, 32);
    let body = multipart_body(BOUNDARY, &[file_part(None, Some("image/png"), &img)]);
    let (status, _, json) = send(&r, upload_request(chat, Some(&multipart_ct()), body)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json["kind"], "image");
    assert_eq!(json["filename"], "upload");
    let t = &json["img_thumbnail"];
    assert_eq!(t["content_type"], "image/webp");
    assert_eq!(t["width"], 64);
    assert_eq!(t["height"], 32);
    assert!(!t["data_base64"].as_str().unwrap().is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn multipart_errors() {
    let env = TestEnv::default_env().await;
    let r = router(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;

    let cases: Vec<(Option<String>, Vec<u8>, &str, &str)> = vec![
        (None, b"x".to_vec(), "content_type", "BOUNDARY_REQUIRED"),
        (Some("application/json".into()), b"{}".to_vec(), "content_type", "BOUNDARY_REQUIRED"),
        (Some(multipart_ct()), b"garbage without boundary".to_vec(), "multipart", "MULTIPART_ERROR"),
        (
            Some(multipart_ct()),
            multipart_body(BOUNDARY, &[("other", None, Some("text/plain"), b"x")]),
            "file",
            "MISSING_FILE",
        ),
        (
            Some(multipart_ct()),
            multipart_body(BOUNDARY, &[file_part(Some("a.txt"), None, b"x")]),
            "content_type",
            "MISSING_CONTENT_TYPE",
        ),
        (
            Some(multipart_ct()),
            multipart_body(BOUNDARY, &[file_part(Some("a.exe"), Some("application/x-msdownload"), b"x")]),
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
        ),
    ];
    for (ct, body, field, reason) in cases {
        let (status, _, json) = send(&r, upload_request(chat, ct.as_deref(), body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reason}: {json}");
        let v = &json["context"]["field_violations"][0];
        assert_eq!(v["field"], field, "{json}");
        assert_eq!(v["reason"], reason, "{json}");
        assert_eq!(json["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
    }
    env.shutdown().await;
}

#[tokio::test]
async fn oversized_file_is_rejected_while_streaming() {
    let mut opts = TestOptions::default();
    opts.cfg.rag.uploaded_file_max_size_kb = 1;
    let env = TestEnv::new(opts).await;
    let r = router(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let data = vec![b'a'; 1025];
    let body = multipart_body(BOUNDARY, &[file_part(Some("a.txt"), Some("text/plain"), &data)]);
    let (status, _, json) = send(&r, upload_request(chat, Some(&multipart_ct()), body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.out_of_range.v1~");
    let v = &json["context"]["field_violations"][0];
    assert_eq!(v["field"], "content_length");
    assert_eq!(v["reason"], "FILE_TOO_LARGE");
    assert!(env.storage.calls.lock().is_empty());

    // Exactly at the limit is accepted.
    let data = vec![b'a'; 1024];
    let body = multipart_body(BOUNDARY, &[file_part(Some("a.txt"), Some("text/plain"), &data)]);
    let (status, _, _) = send(&r, upload_request(chat, Some(&multipart_ct()), body)).await;
    assert_eq!(status, StatusCode::CREATED);
    env.shutdown().await;
}

#[tokio::test]
async fn concurrency_limit_returns_503_retry_after_5() {
    let mut opts = TestOptions::default();
    opts.cfg.rag.max_concurrent_uploads = 0;
    let env = TestEnv::new(opts).await;
    let r = router(&env);
    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let body = multipart_body(BOUNDARY, &[file_part(Some("a.txt"), Some("text/plain"), b"x")]);
    let (status, headers, json) = send(&r, upload_request(chat, Some(&multipart_ct()), body)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_eq!(headers["retry-after"], "5");
    assert_eq!(json["context"]["retry_after_seconds"], 5);
    env.shutdown().await;
}

#[tokio::test]
async fn unknown_chat_is_404_before_body_and_storage_failure_is_503() {
    let env = TestEnv::default_env().await;
    let r = router(&env);
    // Unknown chat: the (invalid) body is never read.
    let (status, _, json) = send(&r, upload_request(Uuid::new_v4(), None, b"x".to_vec())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");

    // Model gone from the catalog: 400 INVALID_MODEL before the body is read.
    let gone = insert_chat(&env, USER_A1, TENANT_A, "removed").await;
    let (status, _, json) = send(&r, upload_request(gone, None, b"x".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["context"]["field_violations"][0]["reason"], "INVALID_MODEL");

    let chat = insert_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    *env.storage.upload_error.lock() = Some(crate::infra::llm::StorageError::Transport("down".into()));
    let body = multipart_body(BOUNDARY, &[file_part(Some("a.txt"), Some("text/plain"), b"x")]);
    let (status, headers, json) = send(&r, upload_request(chat, Some(&multipart_ct()), body)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{json}");
    assert_eq!(headers["retry-after"], "10");
    assert_eq!(json["detail"], "Service temporarily unavailable");
    env.shutdown().await;
}
