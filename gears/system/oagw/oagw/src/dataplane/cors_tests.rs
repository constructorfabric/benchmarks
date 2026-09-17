// Created: 2026-09-04 by Constructor Tech
//! Tests of the built-in CORS handling of the data plane.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use http::{HeaderMap, HeaderValue, Method};

use super::*;
use crate::domain::{AllowedOrigin, CorsConfig, HttpMethod, SharingMode};

/// A CORS configuration with `origins` and `methods`.
fn cors(origins: &[&str], methods: &[&str], credentials: bool) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: origins
            .iter()
            .map(|origin| {
                if *origin == "*" {
                    AllowedOrigin::Any
                } else {
                    AllowedOrigin::Exact(String::from(*origin))
                }
            })
            .collect(),
        allowed_methods: methods
            .iter()
            .map(|method| HttpMethod::parse(method).unwrap())
            .collect(),
        expose_headers: Vec::new(),
        allow_credentials: credentials,
    }
}

/// A header map built from raw `name: value` pairs.
fn headers_of(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in pairs {
        headers.insert(
            http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_str(value).unwrap(),
        );
    }
    headers
}

#[test]
fn preflight_needs_options_origin_and_request_method() {
    let both = headers_of(&[
        ("origin", "https://app.example"),
        ("access-control-request-method", "POST"),
    ]);
    assert!(is_preflight(&Method::OPTIONS, &both));

    assert!(!is_preflight(&Method::POST, &both));
    let only_origin = headers_of(&[("origin", "https://app.example")]);
    assert!(!is_preflight(&Method::OPTIONS, &only_origin));
    let only_method = headers_of(&[("access-control-request-method", "POST")]);
    assert!(!is_preflight(&Method::OPTIONS, &only_method));
}

#[test]
fn origin_allowlist_matches_case_insensitively_or_any() {
    let config = cors(
        &["HTTPS://App.Example", "https://beta.example"],
        &["GET"],
        false,
    );
    assert!(origin_allowed(&config, "https://app.example"));
    assert!(origin_allowed(&config, "https://beta.example"));
    assert!(!origin_allowed(&config, "https://evil.example"));

    let wildcard = cors(&["*"], &["GET"], false);
    assert!(origin_allowed(&wildcard, "https://anything.example"));
}

#[test]
fn unknown_method_tokens_are_never_allowed() {
    let config = cors(&["*"], &["GET", "POST"], false);
    assert!(method_allowed(&config, &Method::GET));
    assert!(method_allowed(&config, &Method::POST));
    assert!(!method_allowed(&config, &Method::DELETE));
    assert!(!method_allowed(
        &config,
        &Method::from_bytes(b"PROPFIND").unwrap()
    ));
}

#[test]
fn same_origin_requests_are_always_allowed() {
    let config = cors(&["https://app.example"], &["GET"], false);
    assert!(check_actual(&config, None, &Method::GET).is_ok());
}

#[test]
fn cross_origin_requests_outside_the_allowlist_are_rejected() {
    let config = cors(&["https://app.example"], &["GET"], false);

    let origin = check_actual(&config, Some("https://evil.example"), &Method::GET).unwrap_err();
    assert_eq!(
        origin,
        CorsRejection::Origin {
            origin: String::from("https://evil.example"),
        }
    );

    let method = check_actual(&config, Some("https://app.example"), &Method::DELETE).unwrap_err();
    assert_eq!(
        method,
        CorsRejection::Method {
            method: String::from("DELETE"),
        }
    );

    assert!(check_actual(&config, Some("https://app.example"), &Method::GET).is_ok());
}

#[test]
fn preflight_headers_echo_the_request() {
    let headers = preflight_headers(
        "https://app.example",
        "POST",
        Some("content-type, x-custom"),
    );
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://app.example"
    );
    assert_eq!(headers.get("access-control-allow-methods").unwrap(), "POST");
    assert_eq!(
        headers.get("access-control-allow-headers").unwrap(),
        "content-type, x-custom"
    );
    assert_eq!(headers.get("access-control-max-age").unwrap(), "86400");
    assert_eq!(
        headers.get("vary").unwrap(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
}

#[test]
fn preflight_headers_omit_the_requested_headers_when_absent() {
    let headers = preflight_headers("https://app.example", "GET", None);
    assert!(!headers.contains_key("access-control-allow-headers"));
}

#[test]
fn actual_response_headers_wildcard_without_credentials() {
    let config = cors(&["*"], &["GET"], false);
    let mut headers = HeaderMap::new();
    write_response_headers(&config, "https://app.example", &mut headers);
    assert_eq!(headers.get("access-control-allow-origin").unwrap(), "*");
    assert!(!headers.contains_key("access-control-allow-credentials"));
    assert_eq!(headers.get("vary").unwrap(), "Origin");
}

#[test]
fn actual_response_headers_echo_the_origin_with_credentials() {
    let config = cors(&["https://app.example"], &["GET"], true);
    let mut headers = HeaderMap::new();
    write_response_headers(&config, "https://app.example", &mut headers);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://app.example"
    );
    assert_eq!(
        headers.get("access-control-allow-credentials").unwrap(),
        "true"
    );
}

#[test]
fn actual_response_headers_echo_the_origin_for_a_closed_allowlist() {
    let config = cors(&["https://app.example"], &["GET"], false);
    let mut headers = HeaderMap::new();
    write_response_headers(&config, "https://app.example", &mut headers);
    assert_eq!(
        headers.get("access-control-allow-origin").unwrap(),
        "https://app.example"
    );
}

#[test]
fn actual_response_headers_expose_configured_headers() {
    let mut config = cors(&["*"], &["GET"], false);
    config.expose_headers = vec![String::from("X-Request-ID"), String::from("X-Rate")];
    let mut headers = HeaderMap::new();
    write_response_headers(&config, "https://app.example", &mut headers);
    assert_eq!(
        headers.get("access-control-expose-headers").unwrap(),
        "X-Request-ID, X-Rate"
    );
}

#[test]
fn max_age_is_one_day() {
    assert_eq!(MAX_AGE_SECS, 86_400);
}
