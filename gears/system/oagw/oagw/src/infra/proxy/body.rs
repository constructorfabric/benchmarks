//! Request-body validation for the data plane (DESIGN.md "Body validation").

use axum::body::Body;
use axum::http::HeaderMap;
use bytes::Bytes;

use crate::domain::error::DomainError;

/// Hard limit on a proxied request body (DESIGN.md §3.2 "Request size").
pub const MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// Framing a client declared for its request body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BodyFraming {
    /// `Content-Length` header text, when the request carried one.
    pub declared: Option<String>,
    /// `Content-Length` as an unsigned integer, when it parses.
    pub content_length: Option<u64>,
    /// `Transfer-Encoding` header value, lowercased.
    pub transfer_encoding: Option<String>,
}

/// Read the framing headers of `headers`.
#[must_use]
pub fn framing(headers: &HeaderMap) -> BodyFraming {
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .map(str::to_owned);
    let content_length = declared.as_deref().and_then(|raw| raw.parse::<u64>().ok());
    let transfer_encoding = headers
        .get(axum::http::header::TRANSFER_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase);
    BodyFraming {
        declared,
        content_length,
        transfer_encoding,
    }
}

/// Reject a request whose framing the gateway cannot forward verbatim.
///
/// # Errors
///
/// Returns a validation error for a non-chunked transfer encoding or a
/// `Content-Length` that is not an unsigned integer, and
/// `cf.oagw.payload.too_large.v1` when the declared size exceeds the cap.
pub fn validate(framing: &BodyFraming) -> Result<(), DomainError> {
    if let Some(encoding) = &framing.transfer_encoding
        && !encoding.split(',').any(|entry| entry.trim() == "chunked")
    {
        return Err(DomainError::validation(
            "only `chunked` transfer encoding is supported",
        ));
    }
    if let Some(declared) = &framing.declared
        && framing.content_length.is_none()
    {
        return Err(DomainError::validation(format!(
            "`Content-Length` `{declared}` is not a valid unsigned integer"
        )));
    }
    if framing
        .content_length
        .is_some_and(|length| length > MAX_BODY_BYTES)
    {
        return Err(too_large());
    }
    Ok(())
}

/// Buffer the inbound request body, enforcing the cap while reading.
///
/// # Errors
///
/// Returns `cf.oagw.payload.too_large.v1` when the body exceeds
/// [`MAX_BODY_BYTES`] and a validation error when the declared
/// `Content-Length` disagrees with the bytes that arrived.
pub async fn read(body: Body, declared: Option<u64>) -> Result<Bytes, DomainError> {
    if declared.is_some_and(|length| length > MAX_BODY_BYTES) {
        return Err(too_large());
    }
    let limit = usize::try_from(MAX_BODY_BYTES).unwrap_or(usize::MAX);
    let bytes = axum::body::to_bytes(body, limit).await.map_err(|error| {
        if is_body_overflow(&error) {
            too_large()
        } else {
            DomainError::validation(format!("request body could not be read: {error}"))
        }
    })?;
    if let Some(length) = declared {
        let received = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if received != length {
            return Err(DomainError::validation(format!(
                "`Content-Length` {length} does not match the {received} bytes received"
            )));
        }
    }
    Ok(bytes)
}

/// The 413 problem document for an oversized body.
fn too_large() -> DomainError {
    DomainError::payload_too_large(format!(
        "request body exceeds the {MAX_BODY_BYTES} byte limit"
    ))
}

/// Whether an axum body error means the length limit was hit.
fn is_body_overflow(error: &axum::Error) -> bool {
    error
        .to_string()
        .to_ascii_lowercase()
        .contains("length limit")
}

#[cfg(test)]
#[path = "body_tests.rs"]
mod body_tests;
