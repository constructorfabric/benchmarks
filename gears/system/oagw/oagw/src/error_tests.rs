//! Tests for the gateway's error model.

use axum::response::IntoResponse as _;

use crate::error::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, ErrorKind, Extensions,
    OagwError, RateLimitQuota,
};

/// Every documented error kind carries the HTTP status the component's table assigns it.
#[test]
fn status_codes_follow_the_component_table() {
    let cases: &[(ErrorKind, u16)] = &[
        (ErrorKind::ValidationError, 400),
        (ErrorKind::MissingTargetHost, 400),
        (ErrorKind::InvalidTargetHost, 400),
        (ErrorKind::UnknownTargetHost, 400),
        (ErrorKind::AuthenticationFailed, 401),
        (ErrorKind::RouteNotFound, 404),
        (ErrorKind::AlreadyExists, 409),
        (ErrorKind::PluginInUse, 409),
        (ErrorKind::PayloadTooLarge, 413),
        (ErrorKind::RateLimitExceeded, 429),
        (ErrorKind::SecretNotFound, 500),
        (ErrorKind::Internal, 500),
        (ErrorKind::DownstreamError, 502),
        (ErrorKind::ProtocolError, 502),
        (ErrorKind::StreamAborted, 502),
        (ErrorKind::LinkUnavailable, 503),
        (ErrorKind::CircuitBreakerOpen, 503),
        (ErrorKind::PluginNotFound, 503),
        (ErrorKind::ConnectionTimeout, 504),
        (ErrorKind::RequestTimeout, 504),
        (ErrorKind::IdleTimeout, 504),
    ];
    for (kind, expected) in cases {
        assert_eq!(kind.status(), *expected, "{kind:?} carries the wrong status");
    }
}

/// Each kind has its own GTS identifier and a non-empty human title.
#[test]
fn every_kind_has_a_distinct_gts_identifier_and_title() {
    let all = [
        ErrorKind::ValidationError,
        ErrorKind::MissingTargetHost,
        ErrorKind::InvalidTargetHost,
        ErrorKind::UnknownTargetHost,
        ErrorKind::AuthenticationFailed,
        ErrorKind::RouteNotFound,
        ErrorKind::AlreadyExists,
        ErrorKind::PluginInUse,
        ErrorKind::PayloadTooLarge,
        ErrorKind::RateLimitExceeded,
        ErrorKind::SecretNotFound,
        ErrorKind::DownstreamError,
        ErrorKind::ProtocolError,
        ErrorKind::StreamAborted,
        ErrorKind::LinkUnavailable,
        ErrorKind::CircuitBreakerOpen,
        ErrorKind::PluginNotFound,
        ErrorKind::ConnectionTimeout,
        ErrorKind::RequestTimeout,
        ErrorKind::IdleTimeout,
        ErrorKind::Internal,
    ];
    let mut seen = std::collections::HashSet::new();
    for kind in all {
        let id = kind.gts_id();
        assert!(id.starts_with("gts.cf.core.errors.err.v1~"), "{id}");
        assert!(seen.insert(id.to_owned()), "duplicate identifier {id}");
        assert!(!kind.title().is_empty(), "{kind:?} has no title");
    }
}

#[test]
fn the_problem_document_is_an_rfc9457_object() {
    let err = OagwError::new(ErrorKind::RouteNotFound, "no route matches");
    let problem = err.to_problem(Some("/oagw/v1/proxy/alpha/items".to_owned()));
    assert!(problem.problem_type.contains("cf.oagw.route.not_found"));
    assert_eq!(problem.status, 404);
    assert_eq!(problem.detail, "no route matches");
    assert_eq!(problem.instance.as_deref(), Some("/oagw/v1/proxy/alpha/items"));
}

#[test]
fn the_request_context_reaches_the_problem_document() {
    let err = OagwError::new(ErrorKind::LinkUnavailable, "dial failed")
        .with_upstream_id("gts.cf.core.oagw.upstream.v1~abc")
        .with_host("payments.example.com")
        .with_path("/v1/charge")
        .with_retry_after(7);
    let context = err.to_problem(None).context;
    assert_eq!(context["upstream_id"], "gts.cf.core.oagw.upstream.v1~abc");
    assert_eq!(context["host"], "payments.example.com");
    assert_eq!(context["path"], "/v1/charge");
    assert_eq!(context["retry_after_seconds"], 7);
}

#[test]
fn a_throttled_request_carries_retry_after() {
    let err = OagwError::new(ErrorKind::RateLimitExceeded, "too many requests")
        .with_retry_after(12);
    let response = err.into_response();
    assert_eq!(response.status(), 429);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok()),
        Some("12")
    );
}

/// ADR-0003 defaults `response_headers` on: the quota travels in the headers a client
/// reads, and the same figures appear in the problem document's `context`.
#[test]
fn a_throttled_request_reports_its_quota_in_headers_and_context() {
    let err = OagwError::new(ErrorKind::RateLimitExceeded, "too many requests")
        .with_retry_after(12)
        .with_rate_limit(RateLimitQuota {
            limit: 100,
            remaining: 0,
            reset_in_secs: 12,
        });
    let response = err.clone().into_response();
    assert_eq!(response.status(), 429);
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(header("x-ratelimit-limit").as_deref(), Some("100"));
    assert_eq!(header("x-ratelimit-remaining").as_deref(), Some("0"));
    assert_eq!(header("x-ratelimit-reset").as_deref(), Some("12"));
    assert_eq!(header(axum::http::header::RETRY_AFTER.as_str()).as_deref(), Some("12"));

    let context = err.to_problem(None).context;
    assert_eq!(context["rate_limit_limit"], 100);
    assert_eq!(context["rate_limit_remaining"], 0);
    assert_eq!(context["rate_limit_reset_in_secs"], 12);
}

/// No quota attached, no quota headers: the extension stays opt-in per response.
#[test]
fn an_error_without_a_quota_carries_no_rate_limit_headers() {
    let response = OagwError::new(ErrorKind::ValidationError, "bad").into_response();
    assert!(response.headers().get("x-ratelimit-limit").is_none());
    assert!(response.headers().get("x-ratelimit-remaining").is_none());
}

#[test]
fn a_gateway_generated_response_names_itself_as_the_source() {
    let response = OagwError::new(ErrorKind::ValidationError, "bad").into_response();
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    assert_ne!(ERROR_SOURCE_GATEWAY, ERROR_SOURCE_UPSTREAM);
}

#[test]
fn the_problem_document_is_served_as_problem_json() {
    let response = OagwError::new(ErrorKind::ValidationError, "bad").into_response();
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        content_type.starts_with("application/problem+json"),
        "got {content_type}"
    );
}

#[tokio::test]
async fn the_body_of_a_gateway_error_is_a_json_object_with_type_and_status() {
    let response = OagwError::new(ErrorKind::UnknownTargetHost, "unknown host")
        .with_path("/x")
        .into_response();
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await
        .expect("the body is readable")
        .to_bytes();
    let parsed: serde_json::Value = serde_json::from_slice(&body).expect("problem json");
    assert!(parsed.get("type").is_some());
    assert_eq!(parsed["status"], 400);
    assert_eq!(parsed["detail"], "unknown host");
    assert_eq!(parsed["context"]["path"], "/x");
}

#[test]
fn every_kind_maps_onto_the_canonical_catalog() {
    let all = [
        ErrorKind::ValidationError,
        ErrorKind::MissingTargetHost,
        ErrorKind::InvalidTargetHost,
        ErrorKind::UnknownTargetHost,
        ErrorKind::AuthenticationFailed,
        ErrorKind::RouteNotFound,
        ErrorKind::AlreadyExists,
        ErrorKind::PluginInUse,
        ErrorKind::PayloadTooLarge,
        ErrorKind::RateLimitExceeded,
        ErrorKind::SecretNotFound,
        ErrorKind::DownstreamError,
        ErrorKind::ProtocolError,
        ErrorKind::StreamAborted,
        ErrorKind::LinkUnavailable,
        ErrorKind::CircuitBreakerOpen,
        ErrorKind::PluginNotFound,
        ErrorKind::ConnectionTimeout,
        ErrorKind::RequestTimeout,
        ErrorKind::IdleTimeout,
        ErrorKind::Internal,
    ];
    for kind in all {
        let canonical = OagwError::new(kind, "detail").to_canonical();
        let status = canonical.status_code();
        assert_eq!(
            status,
            kind.status(),
            "{kind:?} loses its documented status through the canonical model"
        );
    }
}

#[test]
fn the_error_displays_its_detail_and_identifier() {
    let err = OagwError::new(ErrorKind::PluginNotFound, "no such plugin");
    let rendered = err.to_string();
    assert!(rendered.contains("no such plugin"));
    assert!(rendered.contains("cf.core.errors.err.v1~cf.oagw"));
}

#[test]
fn extensions_default_to_being_absent() {
    let extensions = Extensions::default();
    assert!(extensions.to_json().is_empty());
}

/// FR-029: the problem document's `trace_id` extension names the request the failure
/// belongs to, so it can be followed to the log record and the relayed response.
#[test]
fn the_trace_id_reaches_the_problem_document() {
    let err = OagwError::new(ErrorKind::DownstreamError, "the upstream lied")
        .with_trace_id("req_abc123");
    let problem = err.to_problem(None);
    assert_eq!(problem.trace_id.as_deref(), Some("req_abc123"));
    assert_eq!(problem.context["trace_id"], "req_abc123");
}

/// A request that was never given an identifier names none, rather than an empty string.
#[test]
fn an_absent_trace_id_is_omitted() {
    let err = OagwError::new(ErrorKind::RouteNotFound, "nothing answers").with_trace_id("");
    let problem = err.to_problem(None);
    assert_eq!(problem.trace_id, None, "an empty identifier is not a trace");
    assert!(problem.context.get("trace_id").is_none(), "{:?}", problem.context);
}
