//! Request body validation
//! (`cpt-cf-oagw-algo-request-proxy-body-validate`,
//! `cpt-cf-oagw-dod-request-proxy-body-validation`).
//!
//! The checks run in two places, and the split is what makes the 100 MB limit
//! enforceable *before* any buffering:
//!
//! * [`preflight`] reads only the declared framing headers and runs before the
//!   handler reads the body, so an oversized declared body is a `413` without
//!   the body ever being read;
//! * [`validate`] runs after the body is read and checks the declared length
//!   against the observed one.
//!
//! No upstream connection and no credential lookup happens in either.

use crate::domain::error::DomainError;

/// The declared body length of a request, or `None` when no
/// `Content-Length` was presented.
///
/// Conflicting duplicate values, and a value that is not an unsigned integer,
/// are rejected: the request is malformed and is not forwarded.
///
/// # Errors
///
/// Returns a validation error naming `content-length` when the values
/// conflict or do not parse.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-1
// `inst-rp-al-body-1` .. `-8`, `inst-rp-validate-7`: the declared and observed
// body framing checks — the declared length, the transfer encoding, the
// pre-buffering limit and the observed agreement.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-6
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-8
pub fn declared_length(headers: &[(String, String)]) -> Result<Option<u64>, DomainError> {
    let values: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name == "content-length")
        .map(|(_, value)| value.trim())
        .collect();
    if values.is_empty() {
        return Ok(None);
    }
    let distinct: Vec<&str> = {
        let mut seen: Vec<&str> = Vec::new();
        for value in values {
            if !seen.contains(&value) {
                seen.push(value);
            }
        }
        seen
    };
    if distinct.len() > 1 {
        return Err(DomainError::field_rejection(
            "content-length",
            "the request carries conflicting `Content-Length` values",
        ));
    }
    let value = distinct[0];
    match value.parse::<u64>() {
        Ok(length) => Ok(Some(length)),
        Err(_) => Err(DomainError::field_rejection(
            "content-length",
            "`Content-Length` is not a valid integer",
        )),
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-2
//

/// The transfer encoding of the request, when one is declared.
///
/// # Errors
///
/// Returns a validation error when `Transfer-Encoding` names anything but
/// `chunked`, or when `Content-Length` is presented at the same time.
pub fn transfer_encoding(headers: &[(String, String)]) -> Result<Option<&str>, DomainError> {
    let values: Vec<&str> = headers
        .iter()
        .filter(|(name, _)| name == "transfer-encoding")
        .map(|(_, value)| value.trim())
        .collect();
    if values.is_empty() {
        return Ok(None);
    }
    if let Some(length) = declared_length(headers)? {
        let _ = length;
        return Err(DomainError::field_rejection(
            "transfer-encoding",
            "the request presents `Content-Length` and `Transfer-Encoding` together",
        ));
    }
    // Every token of every value must be `chunked`: a list that carries
    // `chunked` plus another coding names a transfer coding this gateway strips
    // as hop-by-hop and can never honour, so the request is rejected rather
    // than re-framed with the remaining coding still applied to its bytes.
    let chunked = values.iter().all(|value| {
        value
            .split(',')
            .all(|token| token.trim().eq_ignore_ascii_case("chunked"))
    });
    if !chunked {
        return Err(DomainError::field_rejection(
            "transfer-encoding",
            "only `chunked` transfer encoding is accepted",
        ));
    }
    Ok(Some("chunked"))
}

/// The declared body check that runs before the handler reads the body.
///
/// A declared body over the limit is rejected before any byte is buffered,
/// which is the only way the 100 MB hard limit can be a pre-buffering
/// guarantee.
///
/// # Errors
///
/// Returns the payload-too-large error for an oversized declared body and the
/// validation error for a malformed framing declaration.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-1
// `inst-eh-pl-1` .. `-5`: the rejection entry 2.4 raises carries the limit the
// `413` detail states, and a body-shape or membership failure — an unparsable
// `Content-Length`, an unsupported `Transfer-Encoding`, a length that
// disagrees with the body — is raised as the `400` validation error instead.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-2
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-3
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-7
pub fn preflight(
    method: &str,
    headers: &[(String, String)],
    max_bytes: u64,
) -> Result<(), DomainError> {
    if transfer_encoding(headers)?.is_some() {
        return Ok(());
    }
    if let Some(length) = declared_length(headers)? {
        if length > max_bytes {
            return Err(DomainError::PayloadTooLarge {
                path: None,
                trace_id: None,
                upstream_id: None,
                limit_bytes: Some(max_bytes),
            });
        }
    }
    let _ = method;
    Ok(())
}
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-3
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-2
//

/// The body checks that need the observed body size.
///
/// # Errors
///
/// Returns the payload-too-large error for an oversized observed body and the
/// validation error for a length that disagrees with the declared one, or that
/// is absent where the request method requires a body.
// @cpt-end:cpt-cf-oagw-flow-error-handling-payload-too-large:p1:inst-eh-pl-1
pub fn validate(
    method: &str,
    headers: &[(String, String)],
    observed: usize,
    max_bytes: u64,
) -> Result<(), DomainError> {
    let declared = declared_length(headers)?;
    let observed = observed as u64;
    if observed > max_bytes {
        return Err(DomainError::PayloadTooLarge {
            path: None,
            trace_id: None,
            upstream_id: None,
            limit_bytes: Some(max_bytes),
        });
    }
    if let Some(declared) = declared {
        if declared != observed {
            return Err(DomainError::field_rejection(
                "content-length",
                "`Content-Length` disagrees with the body size",
            ));
        }
        return Ok(());
    }
    if transfer_encoding(headers)?.is_some() {
        return Ok(());
    }
    // A body-bearing request that carries a body without declaring its length
    // is malformed; an empty body carries no framing statement to contradict.
    let requires_body = matches!(method, "POST" | "PUT" | "PATCH");
    if requires_body && observed > 0 {
        return Err(DomainError::field_rejection(
            "content-length",
            "`Content-Length` is absent where the request method requires a body",
        ));
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-request-proxy-body-validate:p1:inst-rp-al-body-1
