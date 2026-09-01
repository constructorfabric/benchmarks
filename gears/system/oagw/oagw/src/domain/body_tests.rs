//! Unit tests for [`super::body`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::validate_request_body;
use crate::domain::error::DomainError;

const LIMIT: u64 = 100 * 1024 * 1024;

#[test]
fn a_body_without_length_headers_is_accepted() {
    let accepted = validate_request_body(None, None, 0, LIMIT).unwrap();
    assert_eq!(accepted.size_bytes, 0);
    assert!(!accepted.chunked);
}

#[test]
fn a_matching_content_length_is_accepted() {
    let accepted = validate_request_body(Some("42"), None, 42, LIMIT).unwrap();
    assert_eq!(accepted.size_bytes, 42);
}

#[test]
fn a_mismatched_content_length_is_rejected() {
    let rejected = validate_request_body(Some("42"), None, 24, LIMIT).unwrap_err();
    assert!(matches!(rejected, DomainError::ValidationError { .. }));
    assert_eq!(rejected.status(), 400);
    assert_eq!(
        rejected.extensions().invalid_value.unwrap(),
        "42"
    );
}

#[test]
fn a_non_numeric_content_length_is_rejected() {
    let rejected = validate_request_body(Some("abc"), None, 0, LIMIT).unwrap_err();
    assert!(matches!(rejected, DomainError::ValidationError { .. }));
}

#[test]
fn an_oversized_body_is_rejected_before_buffering() {
    let rejected = validate_request_body(None, None, LIMIT + 1, LIMIT).unwrap_err();
    match rejected {
        DomainError::PayloadTooLarge { limit_bytes } => assert_eq!(limit_bytes, LIMIT),
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(rejected.status(), 413);
}

#[test]
fn chunked_transfer_encoding_is_supported() {
    let accepted = validate_request_body(None, Some("chunked"), 0, LIMIT).unwrap();
    assert!(accepted.chunked);
}

#[test]
fn unsupported_transfer_encodings_are_rejected() {
    for value in ["gzip", "chunked, gzip", "identity"] {
        let rejected = validate_request_body(None, Some(value), 0, LIMIT).unwrap_err();
        assert!(
            matches!(rejected, DomainError::ValidationError { .. }),
            "'{value}' must be rejected"
        );
        assert_eq!(rejected.status(), 400);
    }
}

#[test]
fn an_empty_content_length_header_is_ignored() {
    let accepted = validate_request_body(Some("   "), None, 0, LIMIT).unwrap();
    assert_eq!(accepted.size_bytes, 0);
}

#[test]
fn a_small_limit_is_enforced_exactly() {
    assert!(validate_request_body(None, None, 10, 10).is_ok());
    assert!(validate_request_body(None, None, 11, 10).is_err());
}
