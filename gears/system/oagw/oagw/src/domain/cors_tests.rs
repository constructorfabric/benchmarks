//! Unit tests for [`super::cors`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    apply_response_headers, check_actual_request, classify, cors_method_label, method_allowed,
    origin_allowed, ACCESS_CONTROL_MAX_AGE, ORIGIN_HEADER,
};
use crate::domain::error::{CorsRejection, DomainError};
use crate::domain::models::{CorsMethod, SharingMode};

fn config() -> crate::domain::models::CorsConfig {
    crate::domain::models::CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec!["https://app.example".to_owned()],
        allowed_methods: vec![CorsMethod::Get, CorsMethod::Post],
        expose_headers: vec!["X-Request-Id".to_owned()],
        allow_credentials: true,
    }
}

#[test]
fn the_origin_header_is_the_wire_name() {
    assert_eq!(ORIGIN_HEADER, "origin");
    assert_eq!(ACCESS_CONTROL_MAX_AGE, "86400");
}

#[test]
fn a_request_without_an_origin_is_same_origin() {
    assert_eq!(classify("GET", None, None), super::CorsRequest::SameOrigin);
    assert_eq!(
        classify("GET", Some("   "), None),
        super::CorsRequest::SameOrigin
    );
}

#[test]
fn an_options_request_with_a_request_method_is_a_preflight() {
    let classified = classify("OPTIONS", Some("https://app.example"), Some("POST"));
    match classified {
        super::CorsRequest::Preflight {
            origin,
            request_method,
            ..
        } => {
            assert_eq!(origin, "https://app.example");
            assert_eq!(request_method, "POST");
        }
        other => panic!("unexpected classification: {other:?}"),
    }
}

#[test]
fn an_options_request_without_a_request_method_is_an_actual_request() {
    let classified = classify("OPTIONS", Some("https://app.example"), None);
    assert_eq!(
        classified,
        super::CorsRequest::Actual {
            origin: "https://app.example".to_owned()
        }
    );
}

#[test]
fn origin_matching_is_exact_and_case_insensitive() {
    let cors = config();
    assert!(origin_allowed(&cors, "https://app.example"));
    assert!(origin_allowed(&cors, "https://APP.example"));
    assert!(!origin_allowed(&cors, "https://evil.example"));
    assert!(!origin_allowed(&cors, "https://app.example.evil.com"));
}

#[test]
fn a_wildcard_origin_admits_everything() {
    let mut cors = config();
    cors.allowed_origins = vec!["*".to_owned()];
    assert!(origin_allowed(&cors, "https://anything.example"));
}

#[test]
fn method_matching_uses_the_wire_labels() {
    let cors = config();
    assert!(method_allowed(&cors, "GET"));
    assert!(method_allowed(&cors, "post"));
    assert!(!method_allowed(&cors, "DELETE"));
    assert_eq!(cors_method_label(CorsMethod::Patch), "PATCH");
}

#[test]
fn a_disallowed_origin_answers_403() {
    let cors = config();
    let request = super::CorsRequest::Actual {
        origin: "https://evil.example".to_owned(),
    };
    let rejected = check_actual_request(Some(&cors), &request, "GET").unwrap_err();
    assert_eq!(rejected.status(), 403);
    match rejected {
        DomainError::CorsForbidden { reason, origin, .. } => {
            assert_eq!(reason, CorsRejection::Origin);
            assert_eq!(origin.as_deref(), Some("https://evil.example"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn a_disallowed_method_answers_403() {
    let cors = config();
    let request = super::CorsRequest::Actual {
        origin: "https://app.example".to_owned(),
    };
    let rejected = check_actual_request(Some(&cors), &request, "DELETE").unwrap_err();
    match rejected {
        DomainError::CorsForbidden { reason, .. } => assert_eq!(reason, CorsRejection::Method),
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn a_disabled_cors_block_is_inert() {
    let mut cors = config();
    cors.enabled = false;
    let request = super::CorsRequest::Actual {
        origin: "https://evil.example".to_owned(),
    };
    assert!(check_actual_request(Some(&cors), &request, "GET").is_ok());
    let mut headers = Vec::new();
    apply_response_headers(Some(&cors), &request, &mut headers);
    assert!(headers.is_empty());
}

#[test]
fn an_absent_cors_block_is_inert() {
    let request = super::CorsRequest::Actual {
        origin: "https://app.example".to_owned(),
    };
    assert!(check_actual_request(None, &request, "GET").is_ok());
}

#[test]
fn response_headers_carry_the_allow_origin_directive() {
    let cors = config();
    let request = super::CorsRequest::Actual {
        origin: "https://app.example".to_owned(),
    };
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    apply_response_headers(Some(&cors), &request, &mut headers);
    assert!(headers
        .iter()
        .any(|(name, value)| name == "access-control-allow-origin"
            && value == "https://app.example"));
    assert!(headers
        .iter()
        .any(|(name, _)| name == "access-control-allow-credentials"));
    assert!(headers
        .iter()
        .any(|(name, value)| name == "access-control-expose-headers"
            && value == "X-Request-Id"));
    assert!(headers.iter().any(|(name, value)| name == "vary" && value == "Origin"));
}

#[test]
fn preflight_requests_bypass_the_actual_request_checks() {
    let cors = config();
    let request = super::CorsRequest::Preflight {
        origin: "https://app.example".to_owned(),
        request_method: "DELETE".to_owned(),
        request_headers: Vec::new(),
    };
    assert!(check_actual_request(Some(&cors), &request, "OPTIONS").is_ok());
}
