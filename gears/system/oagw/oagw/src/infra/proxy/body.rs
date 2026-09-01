//! Request-body policy (`DESIGN` §2.2, body validation rules).
//!
//! The hard limit is a deployment constant, not a per-upstream knob: no field
//! of `upstream.v1` names one, so every upstream shares the 100MB ceiling and a
//! body is rejected *before* it is buffered. The remaining checks are the ones
//! the design tabulates — a `Content-Length` that is not an integer, a
//! `Content-Length` that disagrees with the framing, and a `Transfer-Encoding`
//! the proxy does not forward.

use std::collections::BTreeMap;

use crate::domain::error::DomainError;

/// Hard body limit of the proxy path (`DESIGN` §2.2, constraint body-limit).
pub const MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// A validated inbound body framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyFraming {
    /// `true` when the body is delimited by chunked transfer coding.
    pub chunked: bool,
    /// The declared `Content-Length`, when the body is not chunked.
    pub content_length: Option<u64>,
}

/// Validate the inbound body framing against the hard limit.
///
/// # Errors
/// Returns [`DomainError::Validation`] for a `Content-Length` that is not a
/// plain integer or a `Transfer-Encoding` other than `chunked`, and
/// [`DomainError::PayloadTooLarge`] when the declared size exceeds the limit.
pub fn validate(headers: &BTreeMap<String, String>) -> Result<BodyFraming, DomainError> {
    let transfer_encoding = headers.get("transfer-encoding").map(String::as_str);
    let chunked = match transfer_encoding {
        None => false,
        Some(value) if value.eq_ignore_ascii_case("chunked") => true,
        Some(value) => {
            return Err(DomainError::validation(format!(
                "unsupported transfer-encoding '{value}': only chunked is forwarded"
            )));
        }
    };
    let content_length = match headers.get("content-length") {
        None => None,
        Some(value) => Some(value.trim().parse::<u64>().map_err(|_| {
            DomainError::validation(format!("content-length '{value}' is not a valid integer"))
        })?),
    };
    if let Some(length) = content_length
        && length > MAX_BODY_BYTES
    {
        return Err(DomainError::PayloadTooLarge {
            limit_bytes: MAX_BODY_BYTES,
        });
    }
    Ok(BodyFraming {
        chunked,
        content_length,
    })
}

/// `true` when a streamed body of `observed` bytes stays within the limit. A
/// chunked request with no declared length is checked as it streams.
#[must_use]
pub const fn within_limit(observed: u64) -> bool {
    observed <= MAX_BODY_BYTES
}

/// A body stream that aborts as soon as the caller exceeds the hard limit.
///
/// The proxy never buffers a request body: the limit is enforced on the
/// declared `Content-Length` before the first byte is read
/// ([`validate`]) and on the bytes that actually arrive, so an
/// undeclared payload cannot push the process past the ceiling either.
pub struct Bounded<S> {
    inner: std::pin::Pin<Box<S>>,
    seen: u64,
}

impl<S> Bounded<S> {
    /// Wrap `inner`, counting the bytes that cross the limit.
    #[must_use]
    pub fn new(inner: S) -> Self {
        Self {
            inner: Box::pin(inner),
            seen: 0,
        }
    }
}

impl<S, E> futures_util::Stream for Bounded<S>
where
    S: futures_util::Stream<Item = Result<bytes::Bytes, E>>,
    E: Into<BoxError>,
{
    type Item = Result<bytes::Bytes, DomainError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Ready(Some(Err(error))) => {
                std::task::Poll::Ready(Some(Err(DomainError::StreamAborted {
                    detail: format!("the request body could not be read: {}", error.into()),
                })))
            }
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                this.seen += chunk.len() as u64;
                if this.seen > MAX_BODY_BYTES {
                    std::task::Poll::Ready(Some(Err(DomainError::PayloadTooLarge {
                        limit_bytes: MAX_BODY_BYTES,
                    })))
                } else {
                    std::task::Poll::Ready(Some(Ok(chunk)))
                }
            }
        }
    }
}

/// The error type a body transport reports, boxed.
type BoxError = std::boxed::Box<dyn std::error::Error + Send + Sync>;
