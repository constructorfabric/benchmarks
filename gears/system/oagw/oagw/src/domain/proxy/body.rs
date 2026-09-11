//! Body validation: `Content-Length`/`Transfer-Encoding` well-formedness and
//! consistency, and the 100MB hard size limit enforced before buffering
//! (`cpt-cf-oagw-algo-body-validation`, `cpt-cf-oagw-dod-body-validation`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use axum::body::Body;
use axum::http::HeaderMap;
use axum::http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use bytes::Bytes;
use http_body_util::{BodyExt, Limited};

use crate::error::OagwError;

/// The hard body size limit: 100MB, exactly 104857600 bytes
/// (`cpt-cf-oagw-constraint-body-limit`).
pub const MAX_BODY_BYTES: usize = 104_857_600;

/// Validates `Content-Length` and `Transfer-Encoding` against the request
/// headers alone, before any body byte is read
/// (`cpt-cf-oagw-dod-body-validation`).
///
/// Returns the parsed, declared `Content-Length` when present and within the
/// hard limit.
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] (`400`) when `Content-Length` is
/// not a non-negative integer, both `Content-Length` and `Transfer-Encoding`
/// are present, or `Transfer-Encoding` names anything but `chunked`; returns
/// [`OagwError::payload_too_large`] (`413`) when a declared `Content-Length`
/// exceeds [`MAX_BODY_BYTES`].
// @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-body-headers-fn-01
pub fn validate_body_headers(headers: &HeaderMap) -> Result<Option<u64>, OagwError> {
    let content_length = parse_content_length(headers)?;
    let transfer_encoding = headers.get(TRANSFER_ENCODING);

    if content_length.is_some() && transfer_encoding.is_some() {
        return Err(OagwError::validation_error(
            "Content-Length and Transfer-Encoding must not both be present",
        ));
    }
    if let Some(value) = transfer_encoding {
        validate_transfer_encoding(value)?;
    }
    if let Some(declared) = content_length
        && declared > MAX_BODY_BYTES as u64
    {
        return Err(OagwError::payload_too_large(format!(
            "declared Content-Length {declared} exceeds the {MAX_BODY_BYTES}-byte limit"
        )));
    }
    Ok(content_length)
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<u64>, OagwError> {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .map_err(|_| OagwError::validation_error("Content-Length must be ASCII"))?;
    let parsed: u64 = text.parse().map_err(|_| {
        OagwError::validation_error("Content-Length must be a non-negative integer")
    })?;
    Ok(Some(parsed))
}

fn validate_transfer_encoding(value: &axum::http::HeaderValue) -> Result<(), OagwError> {
    let text = value
        .to_str()
        .map_err(|_| OagwError::validation_error("Transfer-Encoding must be ASCII"))?;
    if text.eq_ignore_ascii_case("chunked") {
        Ok(())
    } else {
        Err(OagwError::validation_error(format!(
            "unsupported Transfer-Encoding '{text}'; only 'chunked' is supported"
        )))
    }
}
// @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-body-headers-fn-01

/// Reads the request body up to [`MAX_BODY_BYTES`], rejecting it with `413`
/// as soon as the limit is crossed (whether declared up front or discovered
/// mid-stream), and rejecting a final byte count that disagrees with a
/// declared `Content-Length` (`cpt-cf-oagw-dod-body-validation`).
///
/// # Errors
///
/// Returns [`OagwError::payload_too_large`] when the body exceeds the
/// limit, or [`OagwError::validation_error`] when the received byte count
/// does not match `declared_length`.
// @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-body-read-fn-01
pub async fn read_limited_body(
    body: Body,
    declared_length: Option<u64>,
) -> Result<Bytes, OagwError> {
    let limited = Limited::new(body, MAX_BODY_BYTES);
    let collected = limited.collect().await.map_err(|_| {
        OagwError::payload_too_large(format!(
            "request body exceeds the {MAX_BODY_BYTES}-byte limit"
        ))
    })?;
    let bytes = collected.to_bytes();

    if let Some(declared) = declared_length
        && bytes.len() as u64 != declared
    {
        return Err(OagwError::validation_error(format!(
            "Content-Length {declared} does not match the {} bytes received",
            bytes.len()
        )));
    }
    Ok(bytes)
}
// @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-body-read-fn-01

#[cfg(test)]
mod tests {
    use super::{MAX_BODY_BYTES, read_limited_body, validate_body_headers};
    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        headers
    }

    #[test]
    fn absent_headers_validate_to_no_declared_length() {
        let headers = HeaderMap::new();
        assert_eq!(
            validate_body_headers(&headers).expect("must validate"),
            None
        );
    }

    #[test]
    fn well_formed_content_length_is_returned() {
        let headers = headers_with(&[("content-length", "32")]);
        assert_eq!(
            validate_body_headers(&headers).expect("must validate"),
            Some(32)
        );
    }

    #[test]
    fn malformed_content_length_is_rejected() {
        let headers = headers_with(&[("content-length", "not-a-number")]);
        let error = validate_body_headers(&headers).expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    // @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-body-headers-conflict-test-01
    #[test]
    fn content_length_and_transfer_encoding_together_are_rejected() {
        let headers = headers_with(&[("content-length", "10"), ("transfer-encoding", "chunked")]);
        let error = validate_body_headers(&headers).expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-body-headers-conflict-test-01

    #[test]
    fn chunked_transfer_encoding_alone_is_accepted() {
        let headers = headers_with(&[("transfer-encoding", "chunked")]);
        assert!(validate_body_headers(&headers).is_ok());
    }

    // @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-body-te-unsupported-test-01
    #[test]
    fn unsupported_transfer_encoding_is_rejected() {
        let headers = headers_with(&[("transfer-encoding", "gzip")]);
        let error = validate_body_headers(&headers).expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }
    // @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-body-te-unsupported-test-01

    // @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-body-declared-over-limit-test-01
    #[test]
    fn declared_length_over_the_limit_is_rejected_as_413_before_buffering() {
        let headers = headers_with(&[("content-length", "104857601")]);
        let error = validate_body_headers(&headers).expect_err("must reject");
        assert_eq!(error.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
    // @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-body-declared-over-limit-test-01

    #[tokio::test]
    async fn a_body_matching_its_declared_length_is_returned_intact() {
        let payload = vec![b'a'; 32];
        let body = Body::from(payload.clone());
        let bytes = read_limited_body(body, Some(32))
            .await
            .expect("must read body");
        assert_eq!(bytes.as_ref(), payload.as_slice());
    }

    #[tokio::test]
    async fn a_body_disagreeing_with_its_declared_length_is_rejected() {
        let body = Body::from(vec![b'a'; 10]);
        let error = read_limited_body(body, Some(32))
            .await
            .expect_err("must reject a mismatched length");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    // @cpt-begin:cpt-cf-oagw-dod-body-validation:p1:inst-body-stream-over-limit-test-01
    #[tokio::test]
    async fn a_streamed_body_crossing_the_limit_is_rejected_as_413() {
        let body = Body::from(vec![0_u8; MAX_BODY_BYTES + 1]);
        let error = read_limited_body(body, None)
            .await
            .expect_err("must reject a body crossing the limit");
        assert_eq!(error.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
    // @cpt-end:cpt-cf-oagw-dod-body-validation:p1:inst-body-stream-over-limit-test-01
}
