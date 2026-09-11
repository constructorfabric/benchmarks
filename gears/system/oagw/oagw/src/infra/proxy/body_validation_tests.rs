//! Tests of the request body framing checks
//! (`cpt-cf-oagw-algo-request-proxy-body-validate`,
//! `cpt-cf-oagw-dod-request-proxy-body-validation`).
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-body-validation:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-request-hardening:p1

use crate::infra::proxy::body_validation::{declared_length, preflight, transfer_encoding, validate};
use crate::domain::DomainError;

fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned())).collect()
}

#[test]
fn an_absent_content_length_declares_nothing() {
    assert_eq!(declared_length(&headers(&[("host", "api.vendor.com")])).expect("parsed"), None);
}

#[test]
fn a_declared_length_is_read() {
    let declared = declared_length(&headers(&[("content-length", " 12 ")]))
        .expect("parsed")
        .expect("declared");
    assert_eq!(declared, 12);
}

#[test]
fn conflicting_duplicate_lengths_are_rejected() {
    let error = declared_length(&headers(&[("content-length", "12"), ("content-length", "13")]))
        .expect_err("conflicting");
    assert!(format!("{error}").contains("content-length"));
}

#[test]
fn a_non_numeric_length_is_rejected() {
    let error = declared_length(&headers(&[("content-length", "abc")])).expect_err("not a number");
    assert!(format!("{error}").contains("content-length"));
}

#[test]
fn a_transfer_encoding_other_than_chunked_is_rejected() {
    let error = transfer_encoding(&headers(&[("transfer-encoding", "gzip")]))
        .expect_err("not chunked");
    assert!(format!("{error}").contains("transfer-encoding"));
}

#[test]
fn a_transfer_encoding_naming_chunked_and_another_coding_is_rejected() {
    // `chunked, gzip` names a coding the gateway strips as hop-by-hop and can
    // never honour: the request is rejected rather than re-framed with the
    // remaining coding still applied to its bytes.
    for value in ["chunked, gzip", "gzip, chunked", "chunked,gzip"] {
        let headers = headers(&[("transfer-encoding", value)]);
        assert!(transfer_encoding(&headers).is_err(), "`{value}` must be rejected");
    }
    // A repeated `chunked` is still the only coding present.
    let headers = headers(&[("transfer-encoding", "chunked, chunked")]);
    assert_eq!(transfer_encoding(&headers).expect("chunked only"), Some("chunked"));
}

#[test]
fn chunked_is_accepted() {
    assert_eq!(
        transfer_encoding(&headers(&[("transfer-encoding", "chunked")])).expect("chunked"),
        Some("chunked")
    );
}

#[test]
fn a_body_declared_with_a_length_and_an_encoding_is_rejected() {
    let error = transfer_encoding(&headers(&[("transfer-encoding", "chunked"), ("content-length", "3")]))
        .expect_err("both framings");
    assert!(format!("{error}").contains("transfer-encoding"));
}

#[test]
fn a_declared_body_over_the_limit_is_rejected_before_it_is_read() {
    let error = preflight("POST", &headers(&[("content-length", "101")]), 100).expect_err("oversized");
    assert!(matches!(error, DomainError::PayloadTooLarge { .. }));
}

#[test]
fn a_chunked_request_is_not_rejected_on_a_declared_length() {
    assert!(preflight("POST", &headers(&[("transfer-encoding", "chunked")]), 100).is_ok());
}

#[test]
fn an_observed_body_over_the_limit_is_rejected() {
    let error = validate("POST", &headers(&[]), 101, 100).expect_err("oversized");
    assert!(matches!(error, DomainError::PayloadTooLarge { .. }));
}

#[test]
fn an_observed_body_that_disagrees_with_the_declared_length_is_rejected() {
    let error = validate("POST", &headers(&[("content-length", "4")]), 3, 100).expect_err("disagreeing");
    assert!(format!("{error}").contains("content-length"));
}

#[test]
fn a_body_without_a_declared_length_is_rejected_where_a_body_is_required() {
    let error = validate("POST", &headers(&[]), 3, 100).expect_err("undeclared");
    assert!(format!("{error}").contains("content-length"));
}

#[test]
fn a_bodyless_method_needs_no_declared_length() {
    assert!(validate("GET", &headers(&[]), 0, 100).is_ok());
    assert!(validate("DELETE", &headers(&[]), 0, 100).is_ok());
}
