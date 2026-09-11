//! Unit tests for the problem+json mapping.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::error::OagwError;
use axum::body::to_bytes;
use std::time::Duration;

#[tokio::test]
async fn gateway_error_response_carries_the_document_and_headers() {
    let err = OagwError::ValidationError("alias mismatch".to_owned());
    let response = gateway_error_response(&err, Some("/oagw/v1/upstreams"));
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(json["title"], "Validation Error");
    assert_eq!(json["status"], 400);
    assert_eq!(json["detail"], "alias mismatch");
    assert_eq!(json["instance"], "/oagw/v1/upstreams");
}

#[tokio::test]
async fn rate_limit_error_carries_retry_after() {
    let err = OagwError::RateLimitExceeded {
        detail: "exhausted".to_owned(),
        retry_after: Duration::from_secs(9),
    };
    let response = gateway_error_response(&err, None);
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("9")
    );
}

#[tokio::test]
async fn markers_are_set_on_both_kinds_of_response() {
    let mut response = Response::new(axum::body::Body::empty());
    with_error_source(&mut response, ERROR_SOURCE_UPSTREAM);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );

    let marked = mark_gateway(Response::new(axum::body::Body::empty()));
    assert_eq!(
        marked
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
}

#[test]
fn document_includes_the_context_fields_when_present() {
    let err = OagwError::RouteNotFound("no upstream".to_owned());
    let document = ProblemDocument::from_error(&err, Some("/oagw/v1/proxy/x"))
        .with_trace_id(Some("trace".to_owned()))
        .with_upstream(Some("up-1".to_owned()), Some("api.example.com".to_owned()))
        .with_path(Some("/v1/chat".to_owned()));
    let json = serde_json::to_value(&document).unwrap();
    assert_eq!(json["upstream_id"], "up-1");
    assert_eq!(json["host"], "api.example.com");
    assert_eq!(json["path"], "/v1/chat");
    assert_eq!(json["trace_id"], "trace");
    assert!(json.get("retry_after_seconds").is_none());
}

#[test]
fn document_omits_absent_context_fields() {
    let err = OagwError::PayloadTooLarge("too big".to_owned());
    let json = serde_json::to_value(ProblemDocument::from_error(&err, None)).unwrap();
    assert!(json.get("instance").is_none());
    assert!(json.get("trace_id").is_none());
    assert!(json.get("upstream_id").is_none());
}

#[test]
fn never_leaks_credential_material_into_a_document() {
    let err = OagwError::SecretNotFound("cred://tenant/sk-supersecret".to_owned());
    let json = ProblemDocument::from_error(&err, None).to_json();
    assert!(
        !json.contains("sk-supersecret"),
        "credential material leaked: {json}"
    );
}

#[tokio::test]
async fn oagw_error_implements_into_response() {
    let response = OagwError::RouteNotFound("missing".to_owned()).into_response();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[test]
fn the_scrubber_removes_credential_shapes() {
    assert_eq!(
        super::scrub("credential 'cred://tenant/sk-supersecret' does not exist"),
        format!(
            "credential '{}/{}' does not exist",
            super::REDACTED,
            super::REDACTED
        )
    );
    assert_eq!(
        super::scrub("Bearer abc123def456 was refused"),
        format!("{} was refused", super::REDACTED)
    );
}

#[test]
fn the_scrubber_leaves_ordinary_text_alone() {
    let detail = "alias 'api.openai.com' is not derivable from the endpoints";
    assert_eq!(super::scrub(detail), detail);
    assert_eq!(
        super::scrub("task-list is not a valid header"),
        "task-list is not a valid header"
    );
}
