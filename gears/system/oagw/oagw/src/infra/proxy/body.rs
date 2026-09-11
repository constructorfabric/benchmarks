//! Request body validation for the proxy data plane.

use bytes::Bytes;
use http::HeaderMap;

use crate::domain::error::DomainError;

/// Declared body size, when the request carries a valid `Content-Length`.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the header is present but not a
/// valid non-negative integer.
pub fn declared_length(headers: &HeaderMap) -> Result<Option<u64>, DomainError> {
    let Some(raw) = headers.get(http::header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    let raw = raw
        .to_str()
        .map_err(|_| DomainError::validation("Content-Length is not valid ASCII"))?;
    if raw.trim().is_empty() {
        return Err(DomainError::validation("Content-Length must not be empty"));
    }
    let parsed: u64 = raw
        .trim()
        .parse()
        .map_err(|_| DomainError::validation("Content-Length is not a valid integer"))?;
    Ok(Some(parsed))
}

/// Enforce the body-size rules.
///
/// # Errors
/// Returns [`DomainError::PayloadTooLarge`] above the hard limit and
/// [`DomainError::Validation`] on a `Content-Length` mismatch.
pub fn validate(headers: &HeaderMap, actual: u64, max_bytes: u64) -> Result<(), DomainError> {
    if let Some(declared) = declared_length(headers)?
        && declared != actual
    {
        return Err(DomainError::validation(format!(
            "Content-Length {declared} does not match the request body size {actual}"
        )));
    }
    if actual > max_bytes {
        return Err(DomainError::PayloadTooLarge);
    }
    Ok(())
}

/// Whether the request carries a chunked body and no length.
#[must_use]
pub fn is_chunked(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::TRANSFER_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"))
}

/// Read the inbound request body up to `max_bytes`.
///
/// The gateway buffers request bodies so the `Content-Length` contract and the
/// body ceiling can both be enforced before a byte is sent upstream. Response
/// bodies are never buffered.
///
/// # Errors
/// Returns [`DomainError::PayloadTooLarge`] when the body exceeds the limit.
pub async fn read_bounded(
    body: axum::body::Body,
    max_bytes: u64,
) -> Result<(Bytes, u64), DomainError> {
    let collected = http_body_util::BodyExt::collect(body)
        .await
        .map_err(|err| DomainError::validation(format!("request body could not be read: {err}")))?;
    let bytes = collected.to_bytes();
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if size > max_bytes {
        return Err(DomainError::PayloadTooLarge);
    }
    Ok((bytes, size))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use http::StatusCode;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn content_length_mismatch_is_rejected() {
        let headers = headers_with(&[("content-length", "100")]);
        let err = validate(&headers, 3, 1024).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn matching_content_length_is_accepted() {
        let headers = headers_with(&[("content-length", "3")]);
        assert!(validate(&headers, 3, 1024).is_ok());
    }

    #[test]
    fn oversized_body_is_rejected() {
        let headers = headers_with(&[("content-length", "4")]);
        let err = validate(&headers, 4, 2).unwrap_err();
        assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1"
        );
    }

    #[test]
    fn non_numeric_content_length_is_rejected() {
        let headers = headers_with(&[("content-length", "abc")]);
        let err = validate(&headers, 0, 1024).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn chunked_detection() {
        assert!(is_chunked(&headers_with(&[(
            "transfer-encoding",
            "chunked"
        )])));
        assert!(!is_chunked(&headers_with(&[("content-length", "1")])));
    }
}
