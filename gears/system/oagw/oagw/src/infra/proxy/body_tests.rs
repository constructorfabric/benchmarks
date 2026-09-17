//! Body-framing validation tests.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};

use super::{MAX_BODY_BYTES, framing, read, validate};
use crate::domain::error::ErrorKind;

fn headers(entries: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in entries {
        map.insert(
            axum::http::header::HeaderName::from_lowercase(name.as_bytes())
                .unwrap_or_else(|error| panic!("header name `{name}`: {error}")),
            HeaderValue::from_str(value).unwrap_or_else(|error| panic!("header value: {error}")),
        );
    }
    map
}

#[test]
fn a_plain_length_delimited_body_is_accepted() {
    let framing = framing(&headers(&[("content-length", "12")]));
    assert_eq!(framing.content_length, Some(12));
    assert!(validate(&framing).is_ok());
}

#[test]
fn a_non_chunked_transfer_encoding_is_rejected() {
    let framing = framing(&headers(&[("transfer-encoding", "gzip, identity")]));
    let error = validate(&framing).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Validation);
    assert_eq!(error.kind.http_status(), 400);
}

#[test]
fn chunked_transfer_encoding_is_accepted() {
    let framing = framing(&headers(&[("transfer-encoding", "chunked")]));
    assert!(validate(&framing).is_ok());
}

#[test]
fn a_non_numeric_content_length_is_rejected() {
    let framing = framing(&headers(&[("content-length", "12, 12")]));
    assert_eq!(framing.declared.as_deref(), Some("12, 12"));
    assert_eq!(framing.content_length, None);
    assert_eq!(validate(&framing).unwrap_err().kind, ErrorKind::Validation);
}

#[test]
fn a_declared_body_over_the_cap_is_too_large() {
    let framing = framing(&headers(&[(
        "content-length",
        &(MAX_BODY_BYTES + 1).to_string(),
    )]));
    let error = validate(&framing).unwrap_err();
    assert_eq!(error.kind, ErrorKind::PayloadTooLarge);
    assert_eq!(error.kind.http_status(), 413);
}

#[tokio::test]
async fn the_body_is_buffered_up_to_the_declared_length() {
    let framing = framing(&headers(&[("content-length", "5")]));
    let bytes = read(Body::from("hello"), framing.content_length)
        .await
        .unwrap_or_else(|error| panic!("body read: {error}"));
    assert_eq!(&bytes[..], b"hello");
}

#[tokio::test]
async fn a_body_that_disagrees_with_its_declared_length_is_rejected() {
    let framing = framing(&headers(&[("content-length", "4")]));
    let error = read(Body::from("hello"), framing.content_length)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Validation);
}

#[tokio::test]
async fn a_declared_length_over_the_cap_is_refused_before_reading() {
    let error = read(Body::from("hello"), Some(MAX_BODY_BYTES + 1))
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::PayloadTooLarge);
}
