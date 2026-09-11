#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the RFC 9457 problem rendering (DESIGN §Error Response Format).

use super::*;
use crate::domain::error::{DomainError, ProblemMeta};
use crate::domain::gts_helpers;

fn problem_of(error: &DomainError) -> axum::http::Response<axum::body::Body> {
    problem_response(error, &ProblemMeta::new(), "/oagw/v1/upstreams/abc")
}

#[test]
fn every_error_is_rendered_as_a_problem_document() {
    let response = problem_of(&DomainError::Validation("alias is not legal".to_string()));
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        response.headers().get("content-type").unwrap(),
        "application/problem+json"
    );
    assert_eq!(source_of(&response).as_deref(), Some(SOURCE_GATEWAY));
}

#[test]
fn the_type_member_is_the_gts_error_identifier() {
    let error = DomainError::RouteNotFound("no route for /v2".to_string());
    let body = problem_body(&error, &ProblemMeta::new(), "/oagw/v1/proxy/a/v2", None);
    assert_eq!(body["type"], gts_helpers::ERR_ROUTE_NOT_FOUND);
    assert_eq!(body["status"], 404);
    assert_eq!(body["instance"], "/oagw/v1/proxy/a/v2");
    assert_eq!(body["title"], error.title());
    assert_eq!(body["detail"], error.detail());
    assert!(
        body["type"].as_str().unwrap().starts_with("gts.cf.core.errors.err.v1~"),
        "{}",
        body["type"]
    );
}

#[test]
fn every_error_maps_onto_the_status_the_design_tabulates() {
    
    let cases: [(u16, DomainError); 11] = [
        (400, DomainError::Validation("v".into())),
        (400, DomainError::TargetHostRequired("h".into())),
        (400, DomainError::TargetHostInvalid("h".into())),
        (400, DomainError::TargetHostUnknown("h".into())),
        (401, DomainError::AuthFailed("bad key".into())),
        (404, DomainError::UpstreamNotFound("nope".into())),
        (409, DomainError::AliasConflict("taken".into())),
        (409, DomainError::PluginInUse("bound".into())),
        (413, DomainError::PayloadTooLarge("10 MB".into())),
        (429, DomainError::RateLimited("1/s".into())),
        (503, DomainError::Disabled("off".into())),
    ];
    for (expected, error) in cases {
        assert_eq!(error.status().as_u16(), expected, "{error:?}");
        let response = problem_of(&error);
        assert_eq!(response.status().as_u16(), expected, "{error:?}");
    }
    assert_eq!(DomainError::Timeout("late".into()).status().as_u16(), 504);
    assert_eq!(DomainError::PluginNotFound("gone".into()).status().as_u16(), 404);
    assert_eq!(DomainError::Downstream("refused".into()).status().as_u16(), 502);
    assert_eq!(DomainError::StreamAborted("cut".into()).status().as_u16(), 502);
    assert_eq!(DomainError::Internal("boom".into()).status().as_u16(), 500);
    assert_eq!(DomainError::CorsOriginDenied("x".into()).status().as_u16(), 403);
    assert_eq!(DomainError::CorsMethodDenied("x".into()).status().as_u16(), 403);
}

#[test]
fn extension_fields_are_emitted_at_the_top_level_and_in_context() {
    let meta = ProblemMeta::new()
        .with_alias("payments.example.com")
        .with_path("/v1/payments")
        .with_retry_after(7)
        .with_code("RATE_LIMIT_EXCEEDED");
    let body = problem_body(&DomainError::RateLimited("too fast".into()), &meta, "/proxy", None);
    assert_eq!(body["alias"], "payments.example.com");
    assert_eq!(body["path"], "/v1/payments");
    assert_eq!(body["retry_after_seconds"], 7);
    assert_eq!(body["code"], "RATE_LIMIT_EXCEEDED");
    let context = &body["context"];
    assert_eq!(context["alias"], "payments.example.com");
    assert_eq!(context["retry_after_seconds"], 7);
}

#[test]
fn a_retriable_error_carries_the_retry_after_header() {
    let meta = ProblemMeta::new().with_retry_after(15);
    let response = problem_response(
        &DomainError::RateLimited("too fast".into()),
        &meta,
        "/oagw/v1/proxy/a",
    );
    assert_eq!(response.status().as_u16(), 429);
    let header = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    assert_eq!(header.as_deref(), Some("15"), "{header:?}");
}

#[test]
fn an_error_without_retry_guidance_omits_the_retry_after_header() {
    let response = problem_response(
        &DomainError::Validation("v".into()),
        &ProblemMeta::new(),
        "/oagw/v1/proxy/a",
    );
    assert!(response.headers().get(header::RETRY_AFTER).is_none());
}

#[test]
fn valid_hosts_are_advertised_for_target_host_failures() {
    let meta = ProblemMeta::new()
        .with_valid_hosts(vec!["a.example.com".to_string()])
        .with_invalid_value("c.example.com");
    let body = problem_body(&DomainError::TargetHostUnknown("c.example.com".into()), &meta, "/p", None);
    assert_eq!(body["valid_hosts"], serde_json::json!(["a.example.com"]));
    assert_eq!(body["invalid_value"], "c.example.com");
}

#[test]
fn the_source_header_is_filled_in_by_the_layer_when_missing() {
    let response = axum::response::Response::<axum::body::Body>::new(axum::body::Body::empty());
    assert!(source_of(&response).is_none(), "the fixture must start unstamped");
    let stamped = error_source_layer(response);
    assert_eq!(source_of(&stamped).as_deref(), Some(SOURCE_GATEWAY));
}

#[test]
fn status_code_returns_the_numeric_value() {
    let error = DomainError::UpstreamNotFound("a".into());
    assert_eq!(status_code(&error), 404);
}

#[test]
fn a_trace_id_is_carried_when_the_platform_supplies_one() {
    let body = problem_body(
        &DomainError::Validation("v".into()),
        &ProblemMeta::new(),
        "/oagw/v1",
        Some("trace-1"),
    );
    assert_eq!(body["trace_id"], "trace-1");
}
