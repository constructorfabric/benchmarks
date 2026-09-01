//! Body framing tests (`DESIGN` §2.2, body validation rules).
#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeMap;

use crate::domain::error::DomainError;
use crate::infra::proxy::body::{MAX_BODY_BYTES, validate, within_limit};

fn headers(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn the_hard_limit_is_the_design_ceiling() {
    assert_eq!(MAX_BODY_BYTES, 100 * 1024 * 1024);
}

#[test]
fn a_plain_body_is_forwarded_as_it_stands() {
    let framing = validate(&headers(&[("content-length", "42")])).unwrap();
    assert!(!framing.chunked);
    assert_eq!(framing.content_length, Some(42));
}

#[test]
fn a_body_with_no_declared_length_is_accepted() {
    let framing = validate(&headers(&[])).unwrap();
    assert!(!framing.chunked);
    assert_eq!(framing.content_length, None);
}

#[test]
fn chunked_bodies_are_the_only_transfer_coding_forwarded() {
    let framing = validate(&headers(&[("transfer-encoding", "chunked")])).unwrap();
    assert!(framing.chunked);
    assert_eq!(framing.content_length, None);

    let framing = validate(&headers(&[("transfer-encoding", "Chunked")])).unwrap();
    assert!(framing.chunked);

    let error = validate(&headers(&[("transfer-encoding", "gzip, chunked")])).unwrap_err();
    let DomainError::Validation { detail } = error else {
        panic!("expected Validation");
    };
    assert!(
        detail.contains("gzip, chunked"),
        "the refusal names the coding"
    );
}

#[test]
fn a_content_length_that_is_not_an_integer_is_rejected() {
    let error = validate(&headers(&[("content-length", "12abc")])).unwrap_err();
    let DomainError::Validation { detail } = error else {
        panic!("expected Validation");
    };
    assert!(detail.contains("12abc"), "the refusal names the value");
}

#[test]
fn a_content_length_is_read_after_trimming() {
    let framing = validate(&headers(&[("content-length", " 42 ")])).unwrap();
    assert_eq!(framing.content_length, Some(42));
}

#[test]
fn a_declared_body_over_the_ceiling_is_rejected_before_it_is_buffered() {
    let error = validate(&headers(&[("content-length", "104857601")])).unwrap_err();
    let DomainError::PayloadTooLarge { limit_bytes } = error else {
        panic!("expected PayloadTooLarge");
    };
    assert_eq!(limit_bytes, MAX_BODY_BYTES);
}

#[test]
fn a_body_at_the_ceiling_is_still_accepted() {
    let framing = validate(&headers(&[("content-length", "104857600")])).unwrap();
    assert_eq!(framing.content_length, Some(MAX_BODY_BYTES));
}

#[test]
fn an_observed_size_over_the_ceiling_stops_the_stream() {
    assert!(within_limit(MAX_BODY_BYTES));
    assert!(!within_limit(MAX_BODY_BYTES + 1));
}
