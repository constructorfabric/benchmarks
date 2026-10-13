//! T069: canonical error contract sweep (RFC 9457 Problem, per-category context) across REST and SSE.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;

use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

fn assert_problem(v: &Value, status: u16, category: &str) {
    assert_eq!(v["status"], status, "{v}");
    for k in ["type", "title", "detail"] {
        assert!(v[k].is_string(), "missing {k}: {v}");
    }
    assert!(v["context"].is_object(), "{v}");
    assert!(v.get("code").is_none(), "no top-level code: {v}");
    assert!(
        v["type"].as_str().unwrap().contains(category),
        "category {category}: {v}"
    );
}

#[tokio::test]
async fn every_category_has_the_canonical_shape() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let rid = h.send(chat, "hi").await.request_id();
    let asst = h.messages(chat).await[1]["id"].as_str().unwrap().to_owned();

    // invalid_argument with field violation
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "POST",
            "/mini-chat/v1/chats",
            Some(json!({"title": " "})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_problem(&v, 400, "invalid_argument");
    assert_field_reason(&v, "title", "INVALID_TITLE");

    // out_of_range
    let mut p = default_policy();
    p["model_catalog"][0]["max_input_tokens"] = json!(5);
    h.set_policy(p);
    let r = h.send(chat, &"long ".repeat(50)).await;
    assert_problem(&r.error, 400, "out_of_range");
    assert_field_reason(&r.error, "content", "INPUT_TOO_LONG");
    h.set_policy(default_policy());

    // not_found with resource type
    let (s, v) = h
        .get(&format!("/mini-chat/v1/chats/{}", Uuid::new_v4()))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_problem(&v, 404, "not_found");
    assert_eq!(
        v["context"]["resource_type"],
        "gts.cf.core.mini_chat.chat.v1~"
    );

    // failed_precondition with violations
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "PUT",
            &format!(
                "/mini-chat/v1/chats/{chat}/messages/{}/reaction",
                h.messages(chat).await[0]["id"].as_str().unwrap()
            ),
            Some(json!({"reaction": "like"})),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_problem(&v, 400, "failed_precondition");
    assert_violation(&v, "reaction_target");
    assert!(v["context"]["violations"][0]["type"].is_string());

    // aborted with reason
    let r = h
        .send_body(chat, json!({"content": "x", "request_id": Uuid::new_v4()}))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h
        .sse(
            &h.ctx(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/turns/{rid}/retry"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_problem(&r.error, 409, "aborted");
    assert_reason(&r.error, "NOT_LATEST_TURN");

    // already_exists with resource name
    let att = h.upload_ok(chat, "a.txt", "text/plain", b"x").await;
    h.send_body(chat, json!({"content": "use", "attachment_ids": [att]}))
        .await;
    let (s, v, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/attachments/{att}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_problem(&v, 409, "already_exists");
    assert_eq!(v["context"]["resource_name"], "attachment_locked");

    // resource_exhausted with violations
    let mut p = default_policy();
    p["default_standard_limits"] =
        json!({"limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 1});
    p["default_premium_limits"] =
        json!({"limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 1});
    h.set_policy(p);
    let r = h.send(chat, "x").await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_problem(&r.error, 429, "resource_exhausted");
    assert_eq!(
        r.error["context"]["violations"][0],
        json!({"subject": "tokens", "description": "quota_exceeded"})
    );
    h.set_policy(default_policy());

    // permission_denied
    h.authz.mode.store(PDP_DENY, Ordering::SeqCst);
    let (s, v) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_problem(&v, 403, "permission_denied");
    assert_reason(&v, "AUTHZ_DENIED");

    // service_unavailable with retry-after
    h.authz.mode.store(PDP_FAIL, Ordering::SeqCst);
    let (s, v, hdr) = h
        .req(
            &h.ctx(),
            "GET",
            &format!("/mini-chat/v1/chats/{chat}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_problem(&v, 503, "service_unavailable");
    assert_eq!(v["context"]["retry_after_seconds"], 5);
    assert_eq!(hdr["retry-after"], "5");
    h.authz.mode.store(PDP_ALLOW, Ordering::SeqCst);

    // upload storage failure: 503 retry 10
    h.provider.fail_file_upload.store(true, Ordering::SeqCst);
    let (s, v) = h.upload(&h.ctx(), chat, "b.txt", "text/plain", b"y").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_problem(&v, 503, "service_unavailable");
    assert_eq!(v["context"]["retry_after_seconds"], 10);
    h.provider.fail_file_upload.store(false, Ordering::SeqCst);

    // internal: no details leaked
    h.policy.fail_snapshot.store(true, Ordering::SeqCst);
    let r = h.send(chat, "x").await;
    assert!(r.status.is_server_error());
    assert!(
        !r.error.to_string().contains("down"),
        "internal detail leaked: {}",
        r.error
    );
    h.policy.fail_snapshot.store(false, Ordering::SeqCst);
    let _ = asst;
}

#[tokio::test]
async fn platform_body_and_path_errors() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    // malformed JSON
    let r = axum::http::Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{"))
        .unwrap();
    let (s, b) = h.raw(&h.ctx(), r).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert!(v.get("code").is_none());
    // missing content type
    let r = axum::http::Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    let (s, _) = h.raw(&h.ctx(), r).await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    // schema mismatch -> 422
    let (s, _, _) = h
        .req(
            &h.ctx(),
            "PATCH",
            &format!("/mini-chat/v1/chats/{chat}"),
            Some(json!({"title": 5})),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let r = h
        .sse(
            &h.ctx(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            Some(json!({"nope": 1})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    // bad path params
    let (s, v) = h.get("/mini-chat/v1/chats/xyz/messages").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("invalid_path_params"), "{v}");
}

#[tokio::test]
async fn streaming_errors_use_sse_error_event_not_problem() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Http {
        status: 502,
        body: json!({"error": {"message": "upstream for file-abcdefghijklmnop"}}),
        retry_after: None,
    });
    let r = h.send(chat, "x").await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "errors after stream start are SSE events"
    );
    let e = r.first("error").unwrap();
    assert_eq!(
        e.as_object().unwrap().len(),
        2,
        "{{code,message}} only: {e}"
    );
    assert!(
        !e["message"]
            .as_str()
            .unwrap()
            .contains("file-abcdefghijklmnop")
    );
    assert_eq!(r.events.last().unwrap().0, "error", "error is terminal");
}
