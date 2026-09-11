//! Inbound and body validation of a proxy request.
//!
//! Covers `cpt-cf-oagw-algo-inbound-validate` and
//! `cpt-cf-oagw-algo-body-validate` and the acceptance rows of
//! `cpt-cf-oagw-dod-inbound-validation` and `cpt-cf-oagw-dod-body-validation`:
//! the query allowlist the matched route holds, the CR/LF header-injection
//! check, the declared-size pre-check before any buffering, the framing
//! defects, the 100MB hard limit read as 100,000,000 bytes, and the
//! `Content-Length` agreement.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::data_plane::validate::{BODY_LIMIT_BYTES, validate_body, validate_inbound};
use oagw::domain::error::ErrorKind;
use oagw::domain::proxy::{MatchedRoute, ProxyContext};
use uuid::Uuid;

const TENANT: Uuid = Uuid::from_u128(0x91);
const ROUTE: Uuid = Uuid::from_u128(0x92);

/// A matched route whose query allowlist the caller states.
fn matched(allowlist: &[&str]) -> MatchedRoute {
    MatchedRoute {
        cors: None,
        tenant_id: TENANT,
        route_id: ROUTE,
        priority: None,
        outbound_path: String::from("/v1/chat"),
        match_pattern: String::from("/v1/things"),
        query_allowlist: allowlist.iter().map(|name| String::from(*name)).collect(),
        rate_limit: None,
        plugins: None,
    }
}

/// A proxy context whose query and headers the caller states.
fn context(query: Option<&str>, headers: &[(&str, &str)]) -> ProxyContext {
    ProxyContext {
        method: String::from("POST"),
        alias: String::from("api.example.com"),
        path_suffix: None,
        query: query.map(String::from),
        headers: headers
            .iter()
            .map(|(name, value)| (String::from(*name), String::from(*value)))
            .collect(),
        target_host: None,
        tenant_id: TENANT,
        subject_id: None,
        correlation: None,
    }
}

/// Whether the failure is the 400 validation one.
fn is_validation(error: &oagw::domain::error::DomainError) -> bool {
    error.kind == ErrorKind::ValidationError
}

#[test]
fn a_query_parameter_the_allowlist_names_is_admitted() {
    let context = context(Some("model=gpt-4"), &[("content-type", "application/json")]);
    assert!(validate_inbound(&context, &matched(&["model"])).is_ok());
}

#[test]
fn a_query_parameter_outside_the_allowlist_is_rejected_with_400() {
    let context = context(Some("model=gpt-4&temperature=1"), &[("content-type", "text/plain")]);
    let error = validate_inbound(&context, &matched(&["model"]))
        .expect_err("temperature is not admitted");
    assert!(is_validation(&error));
    assert!(error.detail.contains("temperature"), "{}", error.detail);
}

#[test]
fn an_empty_allowlist_admits_no_parameter_at_all() {
    let context = context(Some("model=gpt-4"), &[]);
    let error = validate_inbound(&context, &matched(&[]))
        .expect_err("the route admits no parameter");
    assert!(is_validation(&error));
    assert!(error.detail.contains("model"), "{}", error.detail);
}

#[test]
fn a_request_without_a_query_has_nothing_to_reject() {
    let context = context(None, &[]);
    assert!(validate_inbound(&context, &matched(&[])).is_ok());
}

#[test]
fn a_repeated_parameter_is_reported_once_per_distinct_name() {
    let context = context(Some("a=1&a=2&b=3"), &[]);
    let error = validate_inbound(&context, &matched(&[])).expect_err("both names are refused");
    assert!(error.detail.contains('a') && error.detail.contains('b'), "{}", error.detail);
}

#[test]
fn a_header_value_carrying_cr_or_lf_is_a_rejection() {
    let context = context(None, &[("x-injected", "value\r\nX-Evil: 1")]);
    let error = validate_inbound(&context, &matched(&[])).expect_err("the value is injected");
    assert!(is_validation(&error));
    assert!(error.detail.contains("x-injected"), "{}", error.detail);
}

#[test]
fn the_query_allowlist_comparison_is_on_the_decoded_name() {
    // `form_urlencoded` decodes `%6dodel` to `model`, so the allowlist admits
    // it under its decoded name.
    let context = context(Some("%6dodel=gpt-4"), &[]);
    assert!(validate_inbound(&context, &matched(&["model"])).is_ok());
}

#[test]
fn the_body_limit_is_read_as_one_hundred_million_bytes() {
    assert_eq!(BODY_LIMIT_BYTES, 100_000_000);
}

#[test]
fn a_body_at_the_limit_is_admitted_and_one_over_it_is_not() {
    let context = context(None, &[("content-length", "100000000")]);
    let body = vec![0_u8; BODY_LIMIT_BYTES];
    assert!(validate_body(&context, &body).is_ok(), "the limit is inclusive");

    let over = vec![0_u8; BODY_LIMIT_BYTES + 1];
    let error = validate_body(&context, &over).expect_err("one byte over the limit");
    assert_eq!(error.kind, ErrorKind::PayloadTooLarge);
}

#[test]
fn a_declared_size_over_the_limit_is_refused_before_any_buffering() {
    let context = context(None, &[("content-length", "100000001")]);
    let error = validate_body(&context, &[]).expect_err("the declared size alone refuses");
    assert_eq!(error.kind, ErrorKind::PayloadTooLarge);
}

#[test]
fn a_content_length_that_is_not_an_integer_is_a_400() {
    let context = context(None, &[("content-length", "many")]);
    let error = validate_body(&context, &[]).expect_err("the value is not an integer");
    assert!(is_validation(&error));
    assert!(
        error.detail.contains("not a valid integer"),
        "{}",
        error.detail
    );
}

#[test]
fn a_content_length_declared_twice_is_a_400() {
    let context = context(None, &[("content-length", "1"), ("content-length", "2")]);
    let error = validate_body(&context, &[]).expect_err("the length is declared twice");
    assert!(is_validation(&error));
    assert!(
        error.detail.contains("more than once"),
        "{}",
        error.detail
    );
}

#[test]
fn a_content_length_that_disagrees_with_the_body_is_a_400() {
    let context = context(None, &[("content-length", "5")]);
    let error = validate_body(&context, b"hello world")
        .expect_err("the declared length does not describe the body");
    assert!(is_validation(&error));
    assert!(
        error.detail.contains("does not match"),
        "{}",
        error.detail
    );
}

#[test]
fn an_agreeing_content_length_is_admitted() {
    let context = context(None, &[("content-length", "5")]);
    assert!(validate_body(&context, b"hello").is_ok());
}

#[test]
fn chunked_framing_is_admitted_alone() {
    let context = context(None, &[("transfer-encoding", "chunked")]);
    assert!(validate_body(&context, b"payload").is_ok());
}

#[test]
fn a_transfer_encoding_that_is_not_chunked_is_a_400() {
    let context = context(None, &[("transfer-encoding", "gzip")]);
    let error = validate_body(&context, b"payload").expect_err("gzip is not chunked");
    assert!(is_validation(&error));
    assert!(error.detail.contains("not chunked"), "{}", error.detail);
}

#[test]
fn content_length_and_transfer_encoding_on_one_request_is_a_400() {
    let context = context(
        None,
        &[("content-length", "7"), ("transfer-encoding", "chunked")],
    );
    let error = validate_body(&context, b"payload").expect_err("both framings are declared");
    assert!(is_validation(&error));
    assert!(error.detail.contains("both declared"), "{}", error.detail);
}

#[test]
fn a_header_value_carrying_cr_or_lf_refuses_the_body_too() {
    let context = context(None, &[("x-injected", "value\n")]);
    let error = validate_body(&context, &[]).expect_err("the value is injected");
    assert!(is_validation(&error));
    assert!(error.detail.contains("x-injected"), "{}", error.detail);
}

#[test]
fn a_request_without_any_framing_header_is_admitted() {
    let context = context(None, &[]);
    assert!(validate_body(&context, b"anything").is_ok());
}
