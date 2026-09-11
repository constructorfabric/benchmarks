//! Validate the Request Body (`cpt-cf-oagw-algo-proxy-validate-body`).

use axum::http::HeaderMap;
use axum::http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};

use crate::proxy::constants::MAX_BODY_BYTES;

/// A body-validation failure: either the hard size limit (`413`) or a
/// well-formedness/consistency violation (`400`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodyError {
    TooLarge,
    Validation(String),
}

fn parse_content_length(headers: &HeaderMap) -> Result<Option<usize>, BodyError> {
    let mut values = headers.get_all(CONTENT_LENGTH).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    let first_str = first
        .to_str()
        .map_err(|_| BodyError::Validation("Content-Length is not valid UTF-8".to_owned()))?;
    for other in values {
        if other.to_str().ok() != Some(first_str) {
            return Err(BodyError::Validation(
                "conflicting duplicate Content-Length values".to_owned(),
            ));
        }
    }
    if first_str.is_empty()
        || !first_str.bytes().all(|b| b.is_ascii_digit())
        || (first_str.len() > 1 && first_str.starts_with('0'))
    {
        return Err(BodyError::Validation(
            "Content-Length is not a well-formed non-negative integer".to_owned(),
        ));
    }
    let value: usize = first_str.parse().map_err(|_| {
        BodyError::Validation("Content-Length is not a well-formed integer".to_owned())
    })?;
    Ok(Some(value))
}

fn transfer_encoding_is_chunked_only(headers: &HeaderMap) -> Result<bool, BodyError> {
    let Some(te) = headers.get(TRANSFER_ENCODING) else {
        return Ok(false);
    };
    let te_str = te
        .to_str()
        .map_err(|_| BodyError::Validation("Transfer-Encoding is not valid UTF-8".to_owned()))?;
    if te_str.trim().eq_ignore_ascii_case("chunked") {
        Ok(true)
    } else {
        Err(BodyError::Validation(
            "Transfer-Encoding must name only the single token 'chunked'".to_owned(),
        ))
    }
}

/// Header-level pre-check, run *before* any byte is buffered
/// (`inst-proxy-body-if-cl-te` through `inst-proxy-body-if-declared-too-large`).
/// Returns the declared `Content-Length`, if any.
// @cpt-algo:cpt-cf-oagw-algo-proxy-validate-body:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-body-validation:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-cl-te
// @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-return-cl-te
pub(crate) fn precheck_headers(headers: &HeaderMap) -> Result<Option<usize>, BodyError> {
    let has_cl = headers.contains_key(CONTENT_LENGTH);
    let has_te = headers.contains_key(TRANSFER_ENCODING);
    if has_cl && has_te {
        return Err(BodyError::Validation(
            "Content-Length and Transfer-Encoding must not both be present".to_owned(),
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-return-cl-te
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-cl-te

    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-te
    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-te-chunked-only
    transfer_encoding_is_chunked_only(headers)?;
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-te-chunked-only
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-te

    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-cl
    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-cl-format
    let declared = parse_content_length(headers)?;
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-cl-format

    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-declared-too-large
    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-return-declared-too-large
    if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
        return Err(BodyError::TooLarge);
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-return-declared-too-large
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-declared-too-large
    // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-if-cl

    Ok(declared)
}

/// Post-buffering consistency check: the actual byte count must equal a
/// declared `Content-Length` (`inst-proxy-body-cl-actual`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-cl-actual
// @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-return
pub(crate) fn check_actual_length(declared: Option<usize>, actual: usize) -> Result<(), BodyError> {
    if let Some(expected) = declared
        && expected != actual
    {
        return Err(BodyError::Validation(
            "actual body length disagrees with the declared Content-Length".to_owned(),
        ));
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-return
// @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-cl-actual

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::try_from(*k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn both_cl_and_te_is_rejected() {
        let h = headers_with(&[("content-length", "3"), ("transfer-encoding", "chunked")]);
        assert!(precheck_headers(&h).is_err());
    }

    #[test]
    fn te_gzip_is_rejected() {
        let h = headers_with(&[("transfer-encoding", "gzip")]);
        assert!(precheck_headers(&h).is_err());
    }

    #[test]
    fn te_chunked_is_accepted_with_no_declared_length() {
        let h = headers_with(&[("transfer-encoding", "chunked")]);
        assert_eq!(precheck_headers(&h).unwrap(), None);
    }

    #[test]
    fn malformed_content_length_is_rejected() {
        let h = headers_with(&[("content-length", "abc")]);
        assert!(precheck_headers(&h).is_err());
    }

    #[test]
    fn negative_content_length_is_rejected() {
        let h = headers_with(&[("content-length", "-5")]);
        assert!(precheck_headers(&h).is_err());
    }

    #[test]
    fn declared_length_over_limit_is_too_large() {
        let h = headers_with(&[("content-length", "999999999999")]);
        assert_eq!(precheck_headers(&h).unwrap_err(), BodyError::TooLarge);
    }

    #[test]
    fn valid_content_length_is_returned() {
        let h = headers_with(&[("content-length", "42")]);
        assert_eq!(precheck_headers(&h).unwrap(), Some(42));
    }

    #[test]
    fn actual_length_mismatch_is_rejected() {
        assert!(check_actual_length(Some(10), 5).is_err());
    }

    #[test]
    fn actual_length_match_is_accepted() {
        assert!(check_actual_length(Some(10), 10).is_ok());
    }
}
