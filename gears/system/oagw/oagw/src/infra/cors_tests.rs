use axum::http::{HeaderMap, HeaderValue};

use super::{
    PREFLIGHT_MAX_AGE, VARY_ORIGIN, apply_response_headers, check_request, is_cross_origin,
    is_preflight, origin, preflight_response,
};
use crate::domain::error::DomainError;
use crate::domain::model::CorsConfig;

/// The rejection of a request that must not be admitted.
fn rejection(result: Result<(), DomainError>) -> DomainError {
    match result {
        Ok(()) => panic!("the request must have been rejected"),
        Err(error) => error,
    }
}

fn config(origins: &[&str], methods: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing: crate::domain::model::Sharing::Inherit,
        enabled: true,
        allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
        allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
        expose_headers: vec!["x-trace-id".to_owned()],
        allow_credentials: true,
    }
}

fn headers_with(entries: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in entries {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(*name),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    headers
}

#[test]
fn preflight_needs_options_origin_and_request_method() {
    let plain = headers_with(&[]);
    assert!(!is_preflight("OPTIONS", &plain));

    let cross = headers_with(&[("origin", "https://app.example.com")]);
    assert!(!is_preflight("OPTIONS", &cross));
    assert!(!is_preflight("GET", &cross));

    let preflight = headers_with(&[
        ("origin", "https://app.example.com"),
        ("access-control-request-method", "POST"),
    ]);
    assert!(is_preflight("OPTIONS", &preflight));
}

#[test]
fn cross_origin_is_the_presence_of_origin() {
    let plain = headers_with(&[]);
    assert!(!is_cross_origin(&plain));
    let cross = headers_with(&[("origin", "https://app.example.com")]);
    assert!(is_cross_origin(&cross));
}

#[test]
fn origin_is_read_case_insensitively() {
    let headers = headers_with(&[("ORIGIN", "https://app.example.com")]);
    assert_eq!(origin(&headers), Some("https://app.example.com"));
}

#[test]
fn preflight_response_echoes_the_request() {
    let request = headers_with(&[
        ("origin", "https://app.example.com"),
        ("access-control-request-method", "DELETE"),
        ("access-control-request-headers", "x-trace-id"),
    ]);
    let response = preflight_response(&request);
    assert_eq!(response.status(), 204);
    let headers = response.headers();
    assert_eq!(
        headers.get("access-control-allow-origin"),
        Some(&HeaderValue::from_static("https://app.example.com"))
    );
    assert_eq!(
        headers.get("access-control-allow-methods"),
        Some(&HeaderValue::from_static("DELETE"))
    );
    assert_eq!(
        headers.get("access-control-allow-headers"),
        Some(&HeaderValue::from_static("x-trace-id"))
    );
    assert_eq!(
        headers.get("access-control-max-age"),
        Some(&HeaderValue::from_static(PREFLIGHT_MAX_AGE))
    );
    assert_eq!(
        headers.get("vary"),
        Some(&HeaderValue::from_static(VARY_ORIGIN))
    );
}

#[test]
fn same_origin_request_is_not_validated() {
    let headers = headers_with(&[]);
    let cors = config(&["https://app.example.com"], &["GET"]);
    assert!(check_request(&cors, &headers, "GET").is_ok());
}

#[test]
fn allowed_origin_and_method_pass() {
    let headers = headers_with(&[("origin", "https://app.example.com")]);
    let cors = config(&["https://app.example.com"], &["GET"]);
    assert!(check_request(&cors, &headers, "GET").is_ok());
    assert!(check_request(&cors, &headers, "get").is_ok());
}

#[test]
fn wildcard_origin_allows_every_origin() {
    let headers = headers_with(&[("origin", "https://elsewhere.example.org")]);
    let cors = config(&["*"], &["GET"]);
    assert!(check_request(&cors, &headers, "GET").is_ok());
}

#[test]
fn disallowed_origin_is_rejected_with_403() {
    let headers = headers_with(&[("origin", "https://evil.example.com")]);
    let cors = config(&["https://app.example.com"], &["GET"]);
    let error = rejection(check_request(&cors, &headers, "GET"));
    assert_eq!(error.kind.http_status(), 403);
    assert_eq!(
        error.kind.gts_instance(),
        "cf.oagw.cors.origin_not_allowed.v1",
        "unexpected error id: {error}"
    );
}

#[test]
fn disallowed_method_is_rejected_with_403() {
    let headers = headers_with(&[("origin", "https://app.example.com")]);
    let cors = config(&["https://app.example.com"], &["GET"]);
    let error = rejection(check_request(&cors, &headers, "DELETE"));
    assert_eq!(error.kind.http_status(), 403);
    assert_eq!(
        error.kind.gts_instance(),
        "cf.oagw.cors.method_not_allowed.v1",
        "unexpected error id: {error}"
    );
}

#[test]
fn response_headers_are_stamped_for_allowed_origins() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    let mut headers = headers_with(&[("origin", "https://app.example.com")]);
    apply_response_headers(&cors, &mut headers);
    assert_eq!(
        headers.get("access-control-allow-origin"),
        Some(&HeaderValue::from_static("https://app.example.com"))
    );
    assert_eq!(
        headers.get("access-control-expose-headers"),
        Some(&HeaderValue::from_static("x-trace-id"))
    );
    assert_eq!(
        headers.get("access-control-allow-credentials"),
        Some(&HeaderValue::from_static("true"))
    );
}

#[test]
fn response_headers_are_omitted_for_other_origins() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    let mut headers = headers_with(&[("origin", "https://evil.example.com")]);
    apply_response_headers(&cors, &mut headers);
    assert!(headers.get("access-control-allow-origin").is_none());
    assert!(headers.get("access-control-allow-credentials").is_none());
}

#[test]
fn vary_is_appended_for_every_origin() {
    let cors = config(&["https://app.example.com"], &["GET"]);
    let mut headers = headers_with(&[]);
    apply_response_headers(&cors, &mut headers);
    assert_eq!(
        headers.get("vary"),
        Some(&HeaderValue::from_static(VARY_ORIGIN))
    );
}
