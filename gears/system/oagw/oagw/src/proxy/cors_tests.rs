//! Tests for CORS enforcement (ADR-0004).

use axum::http::Method;

use crate::domain::upstream::CorsConfig;
use crate::proxy::cors::{
    apply_to_response, is_preflight, method_allowed, origin_allowed, preflight_response,
};

fn config() -> CorsConfig {
    CorsConfig {
        enabled: true,
        allowed_origins: vec!["https://app.example.com".to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        ..CorsConfig::default()
    }
}

fn preflight(origin: Option<&str>, requested: Option<&str>) -> axum::response::Response {
    let mut headers = http::HeaderMap::new();
    if let Some(origin) = origin {
        headers.insert(http::header::ORIGIN, origin.parse().unwrap());
    }
    if let Some(requested) = requested {
        headers.insert(http::header::ACCESS_CONTROL_REQUEST_METHOD, requested.parse().unwrap());
    }
    preflight_response(&headers)
}

fn header(response: &axum::response::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[test]
fn a_preflight_is_options_with_origin_and_requested_method() {
    let headers = http::HeaderMap::new();
    assert!(!is_preflight(&Method::OPTIONS, &headers));

    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::ORIGIN, "https://app.example.com".parse().unwrap());
    assert!(!is_preflight(&Method::OPTIONS, &headers));

    headers.insert(
        http::header::ACCESS_CONTROL_REQUEST_METHOD,
        "POST".parse().unwrap(),
    );
    assert!(is_preflight(&Method::OPTIONS, &headers));
    assert!(!is_preflight(&Method::POST, &headers));
}

#[test]
fn an_enabled_origin_is_allowed_and_an_unknown_one_is_not() {
    let config = config();
    assert!(origin_allowed(&config, "https://app.example.com"));
    assert!(origin_allowed(&config, "HTTPS://APP.EXAMPLE.COM"));
    assert!(!origin_allowed(&config, "https://evil.example.org"));
    // Ports and schemes are significant.
    assert!(!origin_allowed(&config, "http://app.example.com"));
}

#[test]
fn a_disabled_configuration_denies_every_origin() {
    let mut config = config();
    config.enabled = false;
    assert!(!origin_allowed(&config, "https://app.example.com"));
}

#[test]
fn the_wildcard_allows_every_origin_but_never_with_credentials() {
    let mut config = config();
    config.allowed_origins = vec!["*".to_owned()];
    assert!(origin_allowed(&config, "https://anything.example.net"));

    config.allow_credentials = true;
    assert!(
        !origin_allowed(&config, "https://anything.example.net"),
        "`*` combined with credentials must deny"
    );
}

#[test]
fn cross_origin_methods_are_checked_against_the_allowlist() {
    let cors = config();
    assert!(method_allowed(&cors, &Method::GET));
    assert!(method_allowed(&cors, &Method::POST));
    assert!(!method_allowed(&cors, &Method::DELETE));
    // Method matching is case-insensitive, as HTTP method names are case-sensitive but
    // browsers send canonical capitalisation.
    assert!(method_allowed(&cors, &Method::from_bytes(b"get").unwrap()));
}

/// A preflight is answered locally and permissively: no allowlist is consulted, because
/// a browser preflight carries no credentials and there is no tenant context to resolve
/// an upstream with (ADR-0004).
#[test]
fn a_preflight_is_answered_permissively_from_its_own_headers() {
    let response = preflight(Some("https://app.example.com"), Some("POST"));
    assert_eq!(response.status(), 204);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com"),
        "the requested origin is echoed, whatever the allowlist says"
    );
    assert_eq!(
        header(&response, "access-control-allow-methods").as_deref(),
        Some("POST"),
        "the requested method is echoed, not the allowlist"
    );
    assert_eq!(header(&response, "access-control-max-age").as_deref(), Some("86400"));
    assert_eq!(
        header(&response, "vary").as_deref(),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers")
    );
    assert!(header(&response, "access-control-allow-credentials").is_none());
}

#[test]
fn a_preflight_echoes_the_requested_headers_and_a_disallowed_origin_alike() {
    let mut headers = http::HeaderMap::new();
    headers.insert(http::header::ORIGIN, "https://evil.example.org".parse().unwrap());
    headers.insert(http::header::ACCESS_CONTROL_REQUEST_METHOD, "DELETE".parse().unwrap());
    headers.insert(
        http::header::ACCESS_CONTROL_REQUEST_HEADERS,
        "content-type, authorization".parse().unwrap(),
    );
    let response = preflight_response(&headers);
    assert_eq!(response.status(), 204);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("https://evil.example.org")
    );
    assert_eq!(header(&response, "access-control-allow-methods").as_deref(), Some("DELETE"));
    assert_eq!(
        header(&response, "access-control-allow-headers").as_deref(),
        Some("content-type, authorization")
    );
}

/// No `Origin`, nothing to echo: the answer stays a bare `204`.
#[test]
fn a_preflight_without_an_origin_header_has_nothing_reflected() {
    let response = preflight(None, Some("POST"));
    assert_eq!(response.status(), 204);
    assert!(header(&response, "access-control-allow-origin").is_none());
    assert!(header(&response, "access-control-allow-methods").is_none());
}

/// Enforcement is deferred to the actual request, which is answered with a `403`.
#[test]
fn a_disallowed_cross_origin_request_is_rejected_before_the_upstream() {
    let cors = config();
    assert!(!origin_allowed(&cors, "https://evil.example.org"));
    assert!(
        crate::error::ErrorKind::CorsOriginNotAllowed.status() == 403,
        "an actual request is rejected with 403, not a preflight failure"
    );
    assert_eq!(
        crate::error::ErrorKind::CorsOriginNotAllowed.gts_id(),
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
    );
    assert_eq!(
        crate::error::ErrorKind::CorsMethodNotAllowed.gts_id(),
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
    );
}

/// An actual cross-origin response carries the CORS headers.
#[test]
fn an_allowed_cross_origin_response_is_annotated() {
    let config = config();
    let mut response = axum::http::Response::builder()
        .status(200)
        .body(axum::body::Body::empty())
        .unwrap();
    apply_to_response(&config, "https://app.example.com", &Method::GET, &mut response);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(header(&response, "vary").as_deref(), Some("Origin"));
}

#[test]
fn a_wildcard_configuration_annotates_the_literal_star() {
    let mut config = config();
    config.allowed_origins = vec!["*".to_owned()];
    let mut response = axum::http::Response::builder()
        .status(200)
        .body(axum::body::Body::empty())
        .unwrap();
    apply_to_response(&config, "https://any.example.net", &Method::GET, &mut response);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("*")
    );
}

/// A credentialed response names the origin exactly and declares the exposed headers.
#[test]
fn a_credentialed_response_names_the_origin_and_exposes_headers() {
    let mut config = config();
    config.allow_credentials = true;
    config.expose_headers = vec!["x-request-id".to_owned(), "x-ratelimit-remaining".to_owned()];
    let mut response = axum::http::Response::builder()
        .status(200)
        .body(axum::body::Body::empty())
        .unwrap();
    apply_to_response(&config, "https://app.example.com", &Method::GET, &mut response);
    assert_eq!(
        header(&response, "access-control-allow-origin").as_deref(),
        Some("https://app.example.com")
    );
    assert_eq!(
        header(&response, "access-control-allow-credentials").as_deref(),
        Some("true")
    );
    assert_eq!(
        header(&response, "access-control-expose-headers").as_deref(),
        Some("x-request-id, x-ratelimit-remaining")
    );
}

#[test]
fn a_disallowed_cross_origin_response_is_left_untouched() {
    let mut response = axum::http::Response::builder()
        .status(200)
        .body(axum::body::Body::empty())
        .unwrap();
    apply_to_response(
        &config(),
        "https://evil.example.org",
        &Method::GET,
        &mut response,
    );
    assert!(header(&response, "access-control-allow-origin").is_none());
}

#[test]
fn cors_is_opt_in_per_upstream() {
    let config = CorsConfig::default();
    assert!(!config.enabled);
    assert!(!origin_allowed(&config, "https://app.example.com"));
}
