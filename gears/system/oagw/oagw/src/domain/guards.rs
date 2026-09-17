//! Request body guards (DESIGN §3.5 "Guard Rules").
//!
//! All decisions are taken from the request headers plus, for the length
//! checks, the byte count actually observed while streaming: the proxy never
//! buffers a body to validate it.

use http::HeaderMap;

use crate::error::OagwError;

/// Hard ceiling on a proxied request body: 100 MB.
pub const MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// The declared length of a request body, as far as the headers say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclaredLength {
    /// `Content-Length` present and well formed.
    Exact(u64),
    /// `Transfer-Encoding: chunked` (no `Content-Length`).
    Chunked,
    /// Neither header: the length is unknown until the body ends.
    Unknown,
}

/// Validate the body-related request headers.
///
/// * a `Content-Length` that is not a single well-formed non-negative integer
///   is a 400 (`ValidationError`);
/// * a `Transfer-Encoding` other than `chunked` is a 400;
/// * a declared length above [`MAX_BODY_BYTES`] is a 413 (`PayloadTooLarge`).
///
/// The returned value is what the streaming side has to enforce.
pub fn validate_body_headers(headers: &HeaderMap) -> Result<DeclaredLength, OagwError> {
    validate_transfer_encoding(headers)?;

    let lengths = values(headers, "content-length");
    if lengths.is_empty() {
        return Ok(DeclaredLength::Unknown);
    }

    let mut parsed = Vec::with_capacity(lengths.len());
    for raw in lengths {
        let value = raw.trim();
        let length = value.parse::<u64>().map_err(|_| {
            OagwError::validation(format!(
                "`Content-Length` must be a non-negative integer, got '{value}'"
            ))
            .with_extension("field", serde_json::json!("Content-Length"))
        })?;
        parsed.push(length);
    }

    let first = parsed[0];
    if parsed.iter().any(|length| *length != first) {
        return Err(
            OagwError::validation("conflicting `Content-Length` headers on the request")
                .with_extension("field", serde_json::json!("Content-Length")),
        );
    }

    if first > MAX_BODY_BYTES {
        return Err(too_large(first));
    }

    Ok(DeclaredLength::Exact(first))
}

/// `Transfer-Encoding` other than `chunked` cannot be proxied safely.
///
/// Only the chunked coding has a length the streaming client can agree on with
/// the upstream; any other coding (or a non-chunked final coding) is rejected
/// instead of being forwarded verbatim.
///
/// # Errors
/// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) when the
/// header is present and not `chunked`.
pub fn validate_transfer_encoding(headers: &HeaderMap) -> Result<(), OagwError> {
    let encodings = values(headers, "transfer-encoding");
    if encodings.is_empty() {
        return Ok(());
    }

    // A single `Transfer-Encoding: chunked` (case-insensitive, any list order
    // that still ends in `chunked`) is accepted; anything else is rejected.
    for raw in encodings {
        let mut codings = raw
            .split(',')
            .map(|coding| coding.trim().to_ascii_lowercase());
        let last = codings.next_back().unwrap_or_default();
        if last != "chunked" {
            return Err(OagwError::validation(format!(
                "`Transfer-Encoding: {raw}` is not supported; only `chunked` can be proxied"
            ))
            .with_extension("field", serde_json::json!("Transfer-Encoding")));
        }
    }

    Ok(())
}

/// Enforce the declared length and the global ceiling while streaming.
///
/// Returns the error for the first rule broken: the ceiling wins over a
/// matching declared length, because the proxy must stop reading either way.
///
/// # Errors
/// * [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) — more
///   bytes arrived than `Content-Length` declared.
/// * [`OagwErrorKind::PayloadTooLarge`](crate::error::OagwErrorKind) — the body
///   exceeds [`MAX_BODY_BYTES`].
pub fn check_observed_length(declared: DeclaredLength, observed: u64) -> Result<(), OagwError> {
    if observed > MAX_BODY_BYTES {
        return Err(too_large(observed));
    }

    if let DeclaredLength::Exact(expected) = declared
        && observed > expected
    {
        return Err(OagwError::validation(format!(
            "request body is longer than the declared `Content-Length` of {expected} bytes"
        ))
        .with_extension("field", serde_json::json!("Content-Length")));
    }

    Ok(())
}

/// Cross-check the declared length against the body's own size hint.
///
/// The transport knows the length of a fully buffered body exactly, so a
/// request whose `Content-Length` disagrees with the body it carries can be
/// rejected *before* anything is forwarded instead of failing the stream
/// mid-flight. Bodies with no exact size (chunked, streaming) are only
/// policed while streaming, by [`check_observed_length`].
///
/// # Errors
/// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) when the
/// exact size is known and differs from the declared `Content-Length`.
pub fn check_declared_size(
    declared: DeclaredLength,
    size: impl Fn() -> Option<u64>,
) -> Result<(), OagwError> {
    let (Some(observed), DeclaredLength::Exact(expected)) = (size(), declared) else {
        return Ok(());
    };

    if observed == expected {
        return Ok(());
    }

    Err(OagwError::validation(format!(
        "`Content-Length` declares {expected} bytes but the request carries {observed}"
    ))
    .with_extension("field", serde_json::json!("Content-Length")))
}

fn too_large(observed: u64) -> OagwError {
    OagwError::payload_too_large(format!(
        "request body exceeds the {} byte limit (observed at least {observed})",
        MAX_BODY_BYTES
    ))
    .with_extension("limit", serde_json::json!(MAX_BODY_BYTES))
}

fn values<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OagwErrorKind;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                http::HeaderName::from_lowercase(name.as_bytes()).expect("valid header name"),
                http::HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        map
    }

    #[test]
    fn no_length_headers_mean_unknown_length() {
        assert_eq!(
            validate_body_headers(&headers(&[])).expect("no headers"),
            DeclaredLength::Unknown
        );
    }

    #[test]
    fn a_well_formed_content_length_is_accepted() {
        assert_eq!(
            validate_body_headers(&headers(&[("content-length", "42")])).expect("valid length"),
            DeclaredLength::Exact(42)
        );
        assert_eq!(
            validate_body_headers(&headers(&[("content-length", " 0 ")])).expect("zero"),
            DeclaredLength::Exact(0)
        );
    }

    #[test]
    fn a_malformed_content_length_is_a_400() {
        for value in ["abc", "-1", "1.5", "", "18446744073709551616"] {
            let error =
                validate_body_headers(&headers(&[("content-length", value)])).expect_err(value);
            assert_eq!(error.status().as_u16(), 400, "{value}");
            assert_eq!(error.kind(), OagwErrorKind::ValidationError, "{value}");
        }
    }

    #[test]
    fn conflicting_content_lengths_are_a_400() {
        let error = validate_body_headers(&headers(&[("content-length", "4")]))
            .expect("single header is fine");
        assert_eq!(error, DeclaredLength::Exact(4));

        let map = headers(&[("content-length", "4"), ("content-length", "5")]);
        let error = validate_body_headers(&map).expect_err("conflicting lengths");
        assert_eq!(error.status().as_u16(), 400);
        assert_eq!(error.kind(), OagwErrorKind::ValidationError);
    }

    #[test]
    fn chunked_transfer_encoding_is_accepted() {
        assert_eq!(
            validate_body_headers(&headers(&[("transfer-encoding", "chunked")])).expect("chunked"),
            DeclaredLength::Unknown
        );
        assert_eq!(
            validate_body_headers(&headers(&[("transfer-encoding", "gzip, Chunked")]))
                .expect("chunked is the final coding"),
            DeclaredLength::Unknown
        );
    }

    #[test]
    fn any_other_transfer_encoding_is_a_400() {
        for value in ["identity", "gzip", "chunked, gzip", ""] {
            let error =
                validate_body_headers(&headers(&[("transfer-encoding", value)])).expect_err(value);
            assert_eq!(error.status().as_u16(), 400, "{value}");
            assert_eq!(error.kind(), OagwErrorKind::ValidationError, "{value}");
        }
    }

    #[test]
    fn a_body_above_the_ceiling_is_a_413() {
        let declared = MAX_BODY_BYTES + 1;
        let error = validate_body_headers(&headers(&[("content-length", &declared.to_string())]))
            .expect_err("over the ceiling");
        assert_eq!(error.status().as_u16(), 413);
        assert_eq!(error.kind(), OagwErrorKind::PayloadTooLarge);
    }

    #[test]
    fn exactly_the_ceiling_is_accepted() {
        assert_eq!(
            validate_body_headers(&headers(&[("content-length", &MAX_BODY_BYTES.to_string())]))
                .expect("at the ceiling"),
            DeclaredLength::Exact(MAX_BODY_BYTES)
        );
    }

    #[test]
    fn observed_bytes_above_the_ceiling_are_a_413() {
        let error = check_observed_length(DeclaredLength::Chunked, MAX_BODY_BYTES + 1)
            .expect_err("over the ceiling");
        assert_eq!(error.status().as_u16(), 413);
        assert_eq!(error.kind(), OagwErrorKind::PayloadTooLarge);
    }

    #[test]
    fn observed_bytes_above_the_declared_length_are_a_400() {
        let error = check_observed_length(DeclaredLength::Exact(4), 5).expect_err("mismatch");
        assert_eq!(error.status().as_u16(), 400);
        assert_eq!(error.kind(), OagwErrorKind::ValidationError);

        assert!(check_observed_length(DeclaredLength::Exact(4), 4).is_ok());
        assert!(check_observed_length(DeclaredLength::Exact(4), 0).is_ok());
        assert!(check_observed_length(DeclaredLength::Unknown, 12).is_ok());
    }

    #[test]
    fn a_body_that_disagrees_with_its_content_length_is_rejected_early() {
        let error = check_declared_size(DeclaredLength::Exact(8), || Some(3))
            .expect_err("shorter than declared");
        assert_eq!(error.status().as_u16(), 400);
        assert_eq!(error.kind(), OagwErrorKind::ValidationError);

        let error = check_declared_size(DeclaredLength::Exact(3), || Some(8))
            .expect_err("longer than declared");
        assert_eq!(error.status().as_u16(), 400);

        assert!(check_declared_size(DeclaredLength::Exact(3), || Some(3)).is_ok());
        assert!(check_declared_size(DeclaredLength::Chunked, || Some(3)).is_ok());
        assert!(check_declared_size(DeclaredLength::Unknown, || None).is_ok());
    }
}
