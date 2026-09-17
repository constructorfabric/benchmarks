//! Tests for the wire rendering of a data-plane failure.
use axum::http::StatusCode;
use axum::response::IntoResponse;

use super::{PROBLEM_JSON, ProxyError, source_value};
use crate::infra::proxy::rate_limit::RateDecision;
use crate::infra::proxy::{ErrorSource, ProxyFailure};

fn rate_limited() -> ProxyFailure {
    let decision = RateDecision {
        allowed: false,
        remaining: 0,
        reset_seconds: 12,
        retry_after_seconds: 3,
        scope_key: "upstream:route".to_owned(),
    };
    ProxyFailure::new(
        429,
        crate::domain::plugin::RATE_LIMIT_EXCEEDED,
        "Rate Limit Exceeded",
        "too many requests for this scope",
    )
    .with_header("retry-after", &decision.retry_after_seconds.to_string())
    .with_header("x-ratelimit-limit", "60")
}

#[test]
fn a_failure_renders_as_a_problem_document() {
    let failure = rate_limited();
    let problem = ProxyError::new(failure.clone()).problem();
    assert_eq!(problem.status, 429);
    assert_eq!(problem.title, "Rate Limit Exceeded");
    assert_eq!(problem.problem_type, failure.type_uri);
    assert_eq!(problem.detail, failure.detail);
    assert_eq!(problem.instance, None);
    assert_eq!(problem.context, failure.context);
}

#[test]
fn a_failure_converts_from_the_data_plane_type() {
    let error: ProxyError = ProxyFailure::validation("bad alias").into();
    assert_eq!(error.failure().status, 400);
}

#[test]
fn the_response_is_problem_json_with_the_gateway_source() {
    let response = ProxyError::new(rate_limited()).into_response();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some(PROBLEM_JSON)
    );
    assert_eq!(
        response
            .headers()
            .get(crate::api::rest::error::ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("3")
    );
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("60")
    );
}

#[tokio::test]
async fn the_body_carries_the_gts_type() {
    let response = ProxyError::new(rate_limited()).into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    let document: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
    assert_eq!(document["status"], 429);
    assert_eq!(document["title"], "Rate Limit Exceeded");
    assert!(
        document["type"]
            .as_str()
            .is_some_and(|value| value.starts_with("gts."))
    );
    assert!(document["context"].is_object());
}

#[test]
fn the_error_source_wire_values_are_the_adr_0007_ones() {
    assert_eq!(source_value(ErrorSource::Gateway), "gateway");
    assert_eq!(source_value(ErrorSource::Upstream), "upstream");
}

#[test]
fn a_cloned_failure_keeps_its_headers() {
    let failure = rate_limited();
    let clone = failure.clone();
    assert_eq!(failure.headers.len(), clone.headers.len());
    assert_eq!(failure.context, clone.context);
    assert_eq!(failure.source, clone.source);
}

#[test]
fn an_out_of_range_status_still_renders_a_response() {
    // A status the platform's status code type cannot express falls back to
    // 500 rather than panicking.
    let failure = ProxyFailure::new(60_000, crate::domain::plugin::INTERNAL, "Odd", "odd");
    let response = ProxyError::new(failure).into_response();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
