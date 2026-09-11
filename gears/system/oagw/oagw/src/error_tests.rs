//! Tests for the OAGW error model (RFC 9457 + ADR-0007).

use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use crate::error::{
    AUTHENTICATION_FAILED_TYPE, CONFLICT_TYPE, ErrorExtensions, ErrorKind, GATEWAY_ERROR_SOURCE,
    OagwError, OagwProblem, PROBLEM_JSON, RATE_LIMIT_EXCEEDED_TYPE, ROUTE_NOT_FOUND_TYPE,
    UPSTREAM_ERROR_SOURCE, VALIDATION_ERROR_TYPE, with_error_source,
};

#[test]
fn validation_error_carries_the_documented_gts_type_and_status() {
    let error = OagwError::validation("alias must match the documented pattern");

    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.gts_type(), VALIDATION_ERROR_TYPE);
    assert_eq!(error.status_code(), 400);
    assert_eq!(error.title(), "Validation Error");
    assert!(error.is_client_error());
}

#[test]
fn error_kinds_map_to_the_documented_statuses() {
    // Exhaustive over the vocabulary of DESIGN §3.3 "Error Response Format".
    let cases = [
        (ErrorKind::Validation, 400_u16, StatusCode::BAD_REQUEST),
        (ErrorKind::InvalidTargetHost, 400, StatusCode::BAD_REQUEST),
        (ErrorKind::MissingTargetHost, 400, StatusCode::BAD_REQUEST),
        (ErrorKind::UnknownTargetHost, 400, StatusCode::BAD_REQUEST),
        (
            ErrorKind::AuthenticationFailed,
            401,
            StatusCode::UNAUTHORIZED,
        ),
        (ErrorKind::RouteNotFound, 404, StatusCode::NOT_FOUND),
        (ErrorKind::Conflict, 409, StatusCode::CONFLICT),
        (
            ErrorKind::PayloadTooLarge,
            413,
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            ErrorKind::RateLimitExceeded,
            429,
            StatusCode::TOO_MANY_REQUESTS,
        ),
        (ErrorKind::ProtocolError, 502, StatusCode::BAD_GATEWAY),
        (ErrorKind::DownstreamError, 502, StatusCode::BAD_GATEWAY),
        (
            ErrorKind::PluginNotFound,
            503,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            ErrorKind::LinkUnavailable,
            503,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            ErrorKind::ConnectionTimeout,
            504,
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (ErrorKind::RequestTimeout, 504, StatusCode::GATEWAY_TIMEOUT),
        (ErrorKind::Internal, 500, StatusCode::INTERNAL_SERVER_ERROR),
    ];

    for (kind, status, axum_status) in cases {
        let error = OagwError::new(kind, "detail");
        assert_eq!(error.status_code(), status, "{kind:?}");
        assert_eq!(error.gts_type(), kind.gts_type(), "{kind:?}");
        assert_eq!(error.title(), kind.title(), "{kind:?}");
        assert_eq!(
            StatusCode::from_u16(status).expect("valid status"),
            axum_status
        );
    }
}

#[test]
fn every_error_kind_is_reachable_and_distinct() {
    // `RequestTimeout` and `PluginNotFound` have no S2 caller yet (S4 owns the
    // plugin chain); they are still part of the vocabulary, so they are
    // exercised here.
    let request_timeout =
        OagwError::request_timeout("the request deadline elapsed").with_retry_after_seconds(5);
    assert_eq!(request_timeout.status_code(), 504);
    assert_eq!(
        request_timeout.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
    );
    assert_eq!(request_timeout.extensions().retry_after_seconds, Some(5));

    let plugin = OagwError::plugin_not_found("no plugin `cf.core.oagw.apikey.v1`");
    assert_eq!(plugin.status_code(), 503);
    assert_eq!(plugin.title(), "Plugin Not Found");
}

#[test]
fn rate_limit_and_auth_errors_carry_the_documented_types() {
    let rate =
        OagwError::rate_limit_exceeded("tenant quota exhausted").with_retry_after_seconds(30);
    assert_eq!(rate.gts_type(), RATE_LIMIT_EXCEEDED_TYPE);
    assert_eq!(rate.status_code(), 429);
    assert!(rate.is_client_error(), "429 is a 4xx status");

    let auth = OagwError::authentication_failed("no security context on the request");
    assert_eq!(auth.gts_type(), AUTHENTICATION_FAILED_TYPE);
    assert_eq!(auth.status_code(), 401);
    assert!(auth.is_client_error());
}

#[test]
fn problem_document_serializes_the_rfc9457_members() {
    let error = OagwError::route_not_found("no upstream with this id")
        .with_instance("/oagw/v1/upstreams/abc")
        .with_upstream_id("gts.cf.core.oagw.upstream.v1~abc");

    let problem = error.problem();
    let body = serde_json::to_value(&problem).expect("problem must serialize");

    assert_eq!(body["type"], ROUTE_NOT_FOUND_TYPE);
    assert_eq!(body["title"], "Not Found");
    assert_eq!(body["status"], 404);
    assert_eq!(body["detail"], "no upstream with this id");
    assert_eq!(body["instance"], "/oagw/v1/upstreams/abc");
    assert_eq!(body["upstream_id"], "gts.cf.core.oagw.upstream.v1~abc");
}

#[test]
fn absent_extension_members_are_omitted_from_the_body() {
    let body = serde_json::to_value(OagwError::validation("bad payload").problem())
        .expect("problem must serialize");

    for absent in [
        "instance",
        "upstream_id",
        "plugin_id",
        "referenced_by",
        "alias",
        "host",
        "path",
        "valid_hosts",
        "invalid_value",
        "reason",
        "retry_after_seconds",
        "trace_id",
    ] {
        assert!(
            body.get(absent).is_none(),
            "{absent} must be omitted: {body}"
        );
    }
}

#[test]
fn conflict_error_discriminates_the_reason() {
    let error = OagwError::conflict("alias api.openai.com is already taken")
        .with_alias("api.openai.com")
        .with_reason("ALIAS_CONFLICT");

    assert_eq!(error.gts_type(), CONFLICT_TYPE);
    assert_eq!(error.extensions().reason.as_deref(), Some("ALIAS_CONFLICT"));

    let body = serde_json::to_value(error.problem()).expect("problem must serialize");
    assert_eq!(body["reason"], "ALIAS_CONFLICT");
    assert_eq!(body["alias"], "api.openai.com");
}

#[test]
fn plugin_in_use_conflict_names_the_referencing_resources() {
    // ADR-0001 "Plugin Deletion Behavior": the 409 names the plugin and the
    // resources that still reference it.
    let error = OagwError::conflict("plugin is referenced by 1 upstream(s) and 2 route(s)")
        .with_plugin_id("gts.cf.core.oagw.guard_plugin.v1~550e8400")
        .with_referenced_by(vec![
            "gts.cf.core.oagw.upstream.v1~1".to_owned(),
            "gts.cf.core.oagw.route.v1~2".to_owned(),
            "gts.cf.core.oagw.route.v1~3".to_owned(),
        ])
        .with_reason("PLUGIN_IN_USE");

    let body = serde_json::to_value(error.problem()).expect("problem must serialize");
    assert_eq!(
        body["plugin_id"],
        "gts.cf.core.oagw.guard_plugin.v1~550e8400"
    );
    assert_eq!(
        body["referenced_by"],
        serde_json::json!([
            "gts.cf.core.oagw.upstream.v1~1",
            "gts.cf.core.oagw.route.v1~2",
            "gts.cf.core.oagw.route.v1~3"
        ])
    );
}

#[test]
fn round_trips_through_the_wire_representation() {
    let error = OagwError::validation("bad port")
        .with_invalid_value("0")
        .with_retry_after_seconds(30)
        .with_trace_id("trace-1");

    let encoded = serde_json::to_string(&error.problem()).expect("problem must serialize");
    let decoded: OagwProblem = serde_json::from_str(&encoded).expect("problem must parse");

    assert_eq!(decoded, error.problem());
    assert_eq!(decoded.extensions.trace_id.as_deref(), Some("trace-1"));
    assert_eq!(decoded.extensions.retry_after_seconds, Some(30));
}

#[test]
fn problem_document_parses_back_with_flattened_extensions() {
    let body = json!({
        "type": CONFLICT_TYPE,
        "title": "Conflict",
        "status": 409,
        "detail": "plugin still referenced",
        "instance": "/oagw/v1/plugins/p-1",
        "reason": "PLUGIN_IN_USE",
        "upstream_id": "gts.cf.core.oagw.upstream.v1~u-1"
    });

    let problem: OagwProblem = serde_json::from_value(body).expect("problem must parse");
    assert_eq!(problem.problem_type, CONFLICT_TYPE);
    assert_eq!(problem.status, 409);
    assert_eq!(
        problem.extensions,
        Box::new(ErrorExtensions {
            upstream_id: Some("gts.cf.core.oagw.upstream.v1~u-1".to_owned()),
            reason: Some("PLUGIN_IN_USE".to_owned()),
            ..ErrorExtensions::default()
        })
    );
}

#[test]
fn into_response_sets_problem_json_and_gateway_error_source() {
    let response = OagwError::validation("nope").into_response();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some(GATEWAY_ERROR_SOURCE)
    );
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        Some(PROBLEM_JSON.to_owned())
    );
}

#[test]
fn with_error_source_stamps_successes_without_overriding() {
    let success = axum::http::Response::builder()
        .status(StatusCode::CREATED)
        .body(axum::body::Body::empty())
        .expect("static response");
    let stamped = with_error_source(success);
    assert_eq!(
        stamped
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some(GATEWAY_ERROR_SOURCE)
    );

    let upstream = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header("x-oagw-error-source", UPSTREAM_ERROR_SOURCE)
        .body(axum::body::Body::empty())
        .expect("static response");
    let preserved = with_error_source(upstream);
    assert_eq!(
        preserved
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some(UPSTREAM_ERROR_SOURCE)
    );
}

#[test]
fn display_is_terse_and_stable() {
    let error = OagwError::internal("boom");
    let rendered = error.to_string();
    assert!(rendered.contains("500"), "{rendered}");
    assert!(rendered.contains("boom"), "{rendered}");
}

#[tokio::test]
async fn problem_json_body_is_an_object_with_the_gts_type() {
    let response = OagwError::validation("nope").into_response();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body must be collectable");
    let body: Value = serde_json::from_slice(&bytes).expect("body must be JSON");

    assert_eq!(body["type"], VALIDATION_ERROR_TYPE);
    assert_eq!(body["status"], 400);
}
