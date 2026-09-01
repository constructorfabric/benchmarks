//! Request body validation (DESIGN §3.2 "Body Validation Rules").
//!
//! Three default checks are applied to every proxied request, none of which
//! requires configuration:
//!
//! | Check | Rule | Error |
//! |---|---|---|
//! | `Content-Length` | valid integer when present; must match the real size | 400 |
//! | max size | hard limit of `max_payload_bytes`, rejected before buffering | 413 |
//! | `Transfer-Encoding` | only `chunked` is supported | 400 |

use crate::domain::error::DomainError;

/// Outcome of validating an inbound body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyAcceptance {
    /// Body size in bytes, as declared by `Content-Length` or measured.
    pub size_bytes: u64,
    /// Whether the client announced a chunked transfer encoding.
    pub chunked: bool,
}

/// Validates the declared size and encoding of an inbound request body.
///
/// `declared_content_length` is the raw header value, `transfer_encoding` the
/// raw `Transfer-Encoding` header, `actual_size` the number of bytes the
/// handler actually received.
///
/// # Errors
///
/// Returns [`DomainError::ValidationError`] for a malformed length or an
/// unsupported transfer encoding and [`DomainError::PayloadTooLarge`] when the
/// payload exceeds `max_payload_bytes`.
pub fn validate_request_body(
    declared_content_length: Option<&str>,
    transfer_encoding: Option<&str>,
    actual_size: u64,
    max_payload_bytes: u64,
) -> Result<BodyAcceptance, DomainError> {
    let chunked = transfer_encoding_is_chunked(transfer_encoding)?;

    if let Some(raw) = declared_content_length.map(str::trim).filter(|raw| !raw.is_empty()) {
        let declared = raw.parse::<u64>().map_err(|_| DomainError::ValidationError {
            detail: "Content-Length must be a valid non-negative integer".to_owned(),
            invalid_value: Some(raw.to_owned()),
            alias: None,
        })?;
        if declared != actual_size {
            return Err(DomainError::ValidationError {
                detail: format!(
                    "Content-Length {declared} does not match the request body size {actual_size}"
                ),
                invalid_value: Some(raw.to_owned()),
                alias: None,
            });
        }
    }

    if actual_size > max_payload_bytes {
        return Err(DomainError::PayloadTooLarge {
            limit_bytes: max_payload_bytes,
        });
    }

    Ok(BodyAcceptance {
        size_bytes: actual_size,
        chunked,
    })
}

/// Whether the inbound request declared a chunked transfer encoding.
///
/// Any other transfer coding is rejected (DESIGN: "only `chunked` supported").
fn transfer_encoding_is_chunked(transfer_encoding: Option<&str>) -> Result<bool, DomainError> {
    let Some(raw) = transfer_encoding.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(false);
    };
    let codings: Vec<String> = raw
        .split(',')
        .map(|coding| coding.trim().to_ascii_lowercase())
        .filter(|coding| !coding.is_empty())
        .collect();
    if codings.len() == 1 && codings[0] == "chunked" {
        return Ok(true);
    }
    Err(DomainError::ValidationError {
        detail: format!(
            "Transfer-Encoding '{raw}' is not supported; only 'chunked' can be proxied"
        ),
        invalid_value: Some(raw.to_owned()),
        alias: None,
    })
}

#[cfg(test)]
#[path = "body_tests.rs"]
mod tests;
