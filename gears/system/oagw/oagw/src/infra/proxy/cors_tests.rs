//! CORS policy tests (`ADR`-0004).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use serde_json::json;

use crate::domain::error::DomainError;
use crate::infra::proxy::cors::{CorsPolicy, is_preflight, preflight};

fn policy(json: serde_json::Value) -> CorsPolicy {
    CorsPolicy::new(&serde_json::from_value(json).unwrap())
}

fn headers(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn an_options_with_origin_and_request_method_is_a_preflight() {
    assert!(is_preflight(
        "OPTIONS",
        &headers(&[
            ("origin", "https://app.example"),
            ("access-control-request-method", "POST")
        ])
    ));
    assert!(is_preflight(
        "options",
        &headers(&[
            ("origin", "https://app.example"),
            ("access-control-request-method", "POST")
        ])
    ));
}

#[test]
fn a_preflight_needs_both_the_origin_and_the_requested_method() {
    assert!(!is_preflight(
        "GET",
        &headers(&[("origin", "https://app.example")])
    ));
    assert!(!is_preflight(
        "OPTIONS",
        &headers(&[("origin", "https://app.example")])
    ));
    assert!(!is_preflight(
        "OPTIONS",
        &headers(&[("access-control-request-method", "POST")])
    ));
}

#[test]
fn the_preflight_answer_echoes_what_the_browser_asked_for() {
    let answer = preflight(&headers(&[
        ("origin", "https://app.example"),
        ("access-control-request-method", "PUT"),
        ("access-control-request-headers", "x-trace, x-tenant"),
    ]));
    assert_eq!(answer.allow_origin.as_deref(), Some("https://app.example"));
    assert_eq!(answer.allow_methods.as_deref(), Some("PUT"));
    assert_eq!(answer.allow_headers.as_deref(), Some("x-trace, x-tenant"));
    assert_eq!(answer.max_age, 86_400);
    assert_eq!(
        answer.vary,
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
}

#[test]
fn a_preflight_without_the_extra_header_leaves_it_unanswered() {
    let answer = preflight(&headers(&[("origin", "https://app.example")]));
    assert!(answer.allow_headers.is_none());
}

#[test]
fn an_actual_request_from_an_admitted_origin_is_answered_with_cors_headers() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["GET", "POST"],
        "expose_headers": ["x-request-id", "x-ratelimit-remaining"]
    }));

    let headers = policy
        .response_headers(Some("https://app.example"))
        .expect("admitted");
    let headers = headers.into_headers();
    assert!(headers.contains(&(
        "access-control-allow-origin".to_owned(),
        "https://app.example".to_owned()
    )));
    assert!(headers.contains(&("vary".to_owned(), "Origin".to_owned())));
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "access-control-expose-headers"
                && value == "x-request-id, x-ratelimit-remaining")
    );
    assert!(
        !headers
            .iter()
            .any(|(name, _)| name == "access-control-allow-credentials")
    );
}

#[test]
fn a_wildcard_origin_never_claims_credentials() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allowed_methods": ["GET"],
        "allow_credentials": true
    }));
    let headers = policy
        .response_headers(Some("https://any.example"))
        .expect("any origin");
    let headers = headers.into_headers();
    assert!(headers.contains(&(
        "access-control-allow-origin".to_owned(),
        "https://any.example".to_owned()
    )));
    assert!(
        !headers
            .iter()
            .any(|(name, _)| name == "access-control-allow-credentials")
    );
}

#[test]
fn a_named_origin_list_may_claim_credentials() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["GET"],
        "allow_credentials": true
    }));
    let headers = policy
        .response_headers(Some("https://app.example"))
        .expect("admitted");
    assert!(headers.allow_credentials);
}

#[test]
fn a_same_origin_request_carries_no_cors_headers() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["GET"]
    }));
    assert!(policy.response_headers(None).is_none());
}

#[test]
fn a_disallowed_origin_is_a_403() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["GET", "POST"]
    }));

    let error = policy
        .check(Some("https://evil.example"), "GET")
        .unwrap_err();
    let DomainError::AccessDenied { detail } = error else {
        panic!("expected AccessDenied");
    };
    assert!(
        detail.contains("evil.example"),
        "the refusal names the origin"
    );

    let error = policy
        .check(Some("https://app.example"), "DELETE")
        .unwrap_err();
    let DomainError::AccessDenied { detail } = error else {
        panic!("expected AccessDenied");
    };
    assert!(detail.contains("DELETE"), "the refusal names the method");

    assert!(policy.check(Some("https://app.example"), "post").is_ok());
}

#[test]
fn a_request_without_an_origin_is_neither_cors_nor_refused() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["GET"]
    }));
    assert!(policy.check(None, "GET").is_ok());
    assert!(policy.origin_allowed(None));
}

#[test]
fn a_disabled_policy_admits_everything_and_adds_no_header() {
    let policy = CorsPolicy::disabled();
    assert!(policy.check(Some("https://evil.example"), "TRACE").is_ok());
    assert!(policy.origin_allowed(Some("https://evil.example")));
    assert!(policy.method_allowed("TRACE"));
    assert!(
        policy
            .response_headers(Some("https://evil.example"))
            .is_none()
    );
}

#[test]
fn methods_are_compared_case_insensitively() {
    let policy = policy(json!({
        "enabled": true,
        "allowed_origins": ["https://app.example"],
        "allowed_methods": ["get"]
    }));
    assert!(policy.method_allowed("GET"));
    assert!(policy.method_allowed("get"));
    assert!(!policy.method_allowed("DELETE"));
}
