//! Request body validation.
//!
//! Applied before the body is buffered: an oversized body is rejected as soon
//! as the limit is crossed rather than after it has been read into memory.

use crate::domain::error::DomainError;

/// Hard ceiling applied before buffering.
pub const HARD_LIMIT_BYTES: usize = 100 * 1024 * 1024;

/// Result of validating a body while it is being read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyCheck {
    /// The body is acceptable.
    Accepted,
    /// The declared `Content-Length` does not match the actual size.
    LengthMismatch,
    /// The body exceeds the configured limit.
    TooLarge,
}

/// Validates the declared `Transfer-Encoding`.
///
/// # Errors
/// Returns [`DomainError::Validation`] for any value other than `chunked`.
pub fn validate_transfer_encoding(headers: &http::HeaderMap) -> Result<(), DomainError> {
    if let Some(value) = headers.get(http::header::TRANSFER_ENCODING) {
        let raw = value.to_str().unwrap_or_default().to_ascii_lowercase();
        if raw.trim() != "chunked" {
            return Err(DomainError::Validation(format!(
                "Transfer-Encoding `{raw}` is not supported; only `chunked` is"
            )));
        }
    }
    Ok(())
}

/// Validates a declared `Content-Length` before the body is read.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the header is present but not an
/// integer, or when it is negative.
pub fn validate_content_length_declaration(headers: &http::HeaderMap) -> Result<Option<u64>, DomainError> {
    let Some(value) = headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let raw = value.to_str().unwrap_or_default().trim();
    raw.parse::<u64>().map(Some).map_err(|_| {
        DomainError::Validation(format!("Content-Length `{raw}` is not a valid integer"))
    })
}

/// Classifies an actual body size against the declared length and the limit.
///
/// # Errors
/// Returns [`DomainError::PayloadTooLarge`] when the body crosses the limit and
/// [`DomainError::Validation`] when it disagrees with `Content-Length`.
pub fn check_body_size(
    actual: usize,
    declared: Option<u64>,
    limit: usize,
) -> Result<(), DomainError> {
    if actual > limit {
        return Err(DomainError::PayloadTooLarge(format!(
            "request body of {actual} bytes exceeds the {limit} byte limit"
        )));
    }
    if let Some(declared) = declared
        && declared != actual as u64
    {
        return Err(DomainError::Validation(format!(
            "Content-Length declared {declared} bytes but the body carries {actual}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(headers: &[(&str, &str)]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).expect("valid"),
                http::HeaderValue::from_str(value).expect("valid"),
            );
        }
        map
    }

    #[test]
    fn transfer_encoding_must_be_chunked() {
        assert!(validate_transfer_encoding(&map(&[("transfer-encoding", "chunked")])).is_ok());
        assert!(validate_transfer_encoding(&map(&[])).is_ok());
        let error =
            validate_transfer_encoding(&map(&[("transfer-encoding", "gzip")])).expect_err("bad");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn content_length_must_be_an_integer() {
        assert_eq!(
            validate_content_length_declaration(&map(&[("content-length", "12")]))
                .expect("parses"),
            Some(12)
        );
        assert_eq!(validate_content_length_declaration(&map(&[])).expect("absent"), None);
        let error = validate_content_length_declaration(&map(&[("content-length", "ten")]))
            .expect_err("not an integer");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn mismatched_content_length_is_rejected() {
        let error = check_body_size(5, Some(10), HARD_LIMIT_BYTES).expect_err("mismatch");
        assert_eq!(error.status(), 400);
        assert!(check_body_size(10, Some(10), HARD_LIMIT_BYTES).is_ok());
        assert!(check_body_size(0, None, HARD_LIMIT_BYTES).is_ok());
    }

    #[test]
    fn oversized_body_is_rejected_with_413() {
        let error = check_body_size(HARD_LIMIT_BYTES + 1, None, HARD_LIMIT_BYTES)
            .expect_err("too large");
        assert_eq!(error.status(), 413);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
        );
    }
}
