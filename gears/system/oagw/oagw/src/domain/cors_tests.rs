//! Tests for the CORS behaviour of the proxy path (ADR-0004): the preflight
//! fast path, the origin and method enforcement on actual requests, and the
//! response headers an admitted request carries.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use http::{HeaderMap, HeaderValue, Method, header};
use serde_json::json;

use crate::domain::model::CorsConfig;
use crate::error::{CORS_METHOD_NOT_ALLOWED_TYPE, CORS_ORIGIN_NOT_ALLOWED_TYPE};

use super::{PREFLIGHT_MAX_AGE, apply_response_headers, enforce, is_preflight, preflight_response};

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn config(document: serde_json::Value) -> CorsConfig {
    serde_json::from_value(document).expect("cors document")
}

fn headers(entries: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        headers.insert(
            header::HeaderName::from_lowercase(name.as_bytes()).expect("lowercase header name"),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    headers
}

fn cors() -> CorsConfig {
    config(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET", "POST"],
        "expose_headers": ["X-Request-ID"],
        "allow_credentials": true
    }))
}

// ── Preflight ────────────────────────────────────────────────────────────────

#[test]
fn an_options_request_with_origin_and_request_method_is_a_preflight() {
    assert!(is_preflight(
        &Method::OPTIONS,
        &headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST")
        ])
    ));
    assert!(
        !is_preflight(
            &Method::OPTIONS,
            &headers(&[("origin", "https://app.example.com")])
        ),
        "without the requested method the OPTIONS request is a plain one"
    );
    assert!(
        !is_preflight(
            &Method::POST,
            &headers(&[
                ("origin", "https://app.example.com"),
                ("access-control-request-method", "POST")
            ])
        ),
        "only OPTIONS is a preflight"
    );
}

#[test]
fn a_preflight_is_answered_with_a_permissive_204_that_echoes_the_request() {
    let request = headers(&[
        ("origin", "https://app.example.com"),
        ("access-control-request-method", "POST"),
        (
            "access-control-request-headers",
            "Content-Type, Authorization",
        ),
    ]);
    let response = preflight_response(&request);
    let (parts, body) = response.into_parts();

    assert_eq!(parts.status, http::StatusCode::NO_CONTENT);
    assert_eq!(
        http_body::Body::size_hint(&body).exact(),
        Some(0),
        "a preflight answer carries no body"
    );
    assert_eq!(
        parts.headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("https://app.example.com"))
    );
    assert_eq!(
        parts.headers.get(header::ACCESS_CONTROL_ALLOW_METHODS),
        Some(&HeaderValue::from_static("POST"))
    );
    assert_eq!(
        parts.headers.get(header::ACCESS_CONTROL_ALLOW_HEADERS),
        Some(&HeaderValue::from_static("Content-Type, Authorization"))
    );
    assert_eq!(
        parts.headers.get(header::ACCESS_CONTROL_MAX_AGE),
        Some(&HeaderValue::from_static(PREFLIGHT_MAX_AGE))
    );
    assert_eq!(
        parts.headers.get(header::VARY),
        Some(&HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
        ))
    );
}

#[test]
fn a_preflight_answer_is_permissive_even_for_a_request_cors_would_refuse() {
    // The preflight is answered before any resolution, so it cannot know the
    // policy of the resource it is asked about: the ADR makes it echo the
    // request and defer every decision to the actual request that follows.
    let request = headers(&[
        ("origin", "https://evil.com"),
        ("access-control-request-method", "DELETE"),
    ]);
    let response = preflight_response(&request);
    let (parts, _) = response.into_parts();
    assert_eq!(parts.status, http::StatusCode::NO_CONTENT);
    assert_eq!(
        parts.headers.get(header::ACCESS_CONTROL_ALLOW_METHODS),
        Some(&HeaderValue::from_static("DELETE"))
    );
}

// ── Actual requests ──────────────────────────────────────────────────────────

#[test]
fn a_request_without_an_origin_is_not_a_cors_request() {
    let cors = cors();
    assert_eq!(
        enforce(&cors, &Method::GET, &HeaderMap::new()).expect("no origin, nothing to enforce"),
        None,
        "only cross-origin requests are governed by CORS"
    );
}

#[test]
fn an_actual_request_from_an_allowed_origin_is_admitted() {
    let cors = cors();
    let request = headers(&[("origin", "https://app.example.com")]);
    let origin = enforce(&cors, &Method::POST, &request).expect("allowed origin");
    assert_eq!(
        origin,
        Some(HeaderValue::from_static("https://app.example.com"))
    );
}

#[test]
fn an_actual_request_from_a_disallowed_origin_is_rejected_with_403() {
    let cors = cors();
    let request = headers(&[("origin", "https://evil.com")]);
    let error = enforce(&cors, &Method::POST, &request).expect_err("the origin is not allowed");
    assert_eq!(error.status_code(), 403);
    assert_eq!(error.gts_type(), CORS_ORIGIN_NOT_ALLOWED_TYPE);
    assert_eq!(
        error.detail(),
        "Origin 'https://evil.com' not in allowed origins list"
    );
    assert_eq!(
        error.extensions().invalid_value.as_deref(),
        Some("https://evil.com")
    );
}

#[test]
fn the_origin_match_is_port_and_protocol_sensitive() {
    let cors = cors();
    for origin in ["https://app.example.com:8443", "http://app.example.com"] {
        let request = headers(&[("origin", origin)]);
        let error = enforce(&cors, &Method::GET, &request).expect_err("not the allowed origin");
        assert_eq!(error.gts_type(), CORS_ORIGIN_NOT_ALLOWED_TYPE, "{origin}");
    }
}

#[test]
fn the_wildcard_origin_admits_every_origin() {
    let cors = config(json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allowed_methods": ["GET"]
    }));
    let request = headers(&[("origin", "https://anything.example.org")]);
    assert_eq!(
        enforce(&cors, &Method::GET, &request).expect("the wildcard admits every origin"),
        Some(HeaderValue::from_static("https://anything.example.org"))
    );
}

#[test]
fn a_disallowed_method_is_rejected_with_403() {
    let cors = cors();
    let request = headers(&[("origin", "https://app.example.com")]);
    let error = enforce(&cors, &Method::DELETE, &request).expect_err("DELETE is not allowed");
    assert_eq!(error.status_code(), 403);
    assert_eq!(error.gts_type(), CORS_METHOD_NOT_ALLOWED_TYPE);
    assert_eq!(
        error.detail(),
        "Method 'DELETE' not in allowed methods list"
    );
    assert_eq!(error.extensions().invalid_value.as_deref(), Some("DELETE"));
}

#[test]
fn a_resource_with_cors_disabled_governs_nothing() {
    let disabled = config(json!({ "enabled": false, "allowed_origins": ["*"] }));
    let request = headers(&[("origin", "https://app.example.com")]);
    assert_eq!(
        enforce(&disabled, &Method::DELETE, &request).expect("CORS is disabled"),
        None
    );
}

// ── Response headers ─────────────────────────────────────────────────────────

#[test]
fn an_admitted_request_is_answered_with_the_cors_advertisement() {
    let cors = cors();
    let origin = HeaderValue::from_static("https://app.example.com");
    let mut response = headers(&[("vary", "Accept-Encoding")]);
    apply_response_headers(&cors, &origin, &mut response);

    assert_eq!(
        response.get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&origin)
    );
    assert_eq!(
        response.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS),
        Some(&HeaderValue::from_static("true"))
    );
    assert_eq!(
        response.get(header::ACCESS_CONTROL_EXPOSE_HEADERS),
        Some(&HeaderValue::from_static("X-Request-ID"))
    );
    assert_eq!(
        response.get_all(header::VARY).iter().collect::<Vec<_>>(),
        vec![
            &HeaderValue::from_static("Accept-Encoding"),
            &HeaderValue::from_static("Origin")
        ],
        "Vary: Origin is added, not overwritten"
    );
}

#[test]
fn a_wildcard_origin_is_advertised_as_a_wildcard() {
    let cors = config(json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allowed_methods": ["GET"]
    }));
    let origin = HeaderValue::from_static("https://anything.example.org");
    let mut response = HeaderMap::new();
    apply_response_headers(&cors, &origin, &mut response);

    assert_eq!(
        response.get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&HeaderValue::from_static("*"))
    );
    assert!(
        response
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none(),
        "credentials cannot be combined with the wildcard origin"
    );
}

#[test]
fn a_resource_without_exposed_headers_advertises_none() {
    let cors = config(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example.com"],
        "allowed_methods": ["GET"]
    }));
    let mut response = HeaderMap::new();
    apply_response_headers(
        &cors,
        &HeaderValue::from_static("https://app.example.com"),
        &mut response,
    );
    assert!(
        response
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .is_none()
    );
    assert!(
        response
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .is_none()
    );
}
