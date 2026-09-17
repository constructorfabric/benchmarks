// Created: 2026-09-03 by Constructor Tech
//! Request body plumbing: size limiting for the proxy path.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Frame, SizeHint};

use crate::error::{ErrorKind, OagwError};

/// A request body that refuses to read past a configured ceiling.
///
/// The limit is enforced per frame, so both `Content-Length` framed and
/// chunked uploads are bounded. The flag is shared so the caller can detect
/// truncation after the body has been handed to the transport.
pub struct Limited {
    inner: axum::body::Body,
    remaining: u64,
    over_limit: Arc<AtomicBool>,
}

impl Limited {
    /// Wraps `body`, allowing at most `max_bytes` to be forwarded.
    #[must_use]
    pub fn new(body: axum::body::Body, max_bytes: u64) -> Self {
        Self {
            inner: body,
            remaining: max_bytes,
            over_limit: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A handle that reports whether the ceiling was hit.
    #[must_use]
    pub fn limit_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.over_limit)
    }

    /// Whether the body was truncated because it exceeded the ceiling.
    #[must_use]
    pub fn over_limit(&self) -> bool {
        self.over_limit.load(Ordering::Relaxed)
    }
}

/// Error surfaced by a limited body.
#[derive(Debug)]
pub enum BodyError {
    /// The body exceeded the configured ceiling.
    TooLarge,
    /// The inner body failed.
    Inner(axum::Error),
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("request body exceeds the configured limit"),
            Self::Inner(error) => std::fmt::Display::fmt(&error, f),
        }
    }
}

impl std::error::Error for BodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TooLarge => None,
            Self::Inner(error) => Some(error),
        }
    }
}

impl From<axum::Error> for BodyError {
    fn from(error: axum::Error) -> Self {
        Self::Inner(error)
    }
}

impl hyper::body::Body for Limited {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = &mut *self;
        if this.over_limit() {
            return Poll::Ready(Some(Err(BodyError::TooLarge)));
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    let len = u64::try_from(data.len()).unwrap_or(u64::MAX);
                    if len > this.remaining {
                        this.over_limit.store(true, Ordering::Relaxed);
                        return Poll::Ready(Some(Err(BodyError::TooLarge)));
                    }
                    this.remaining -= len;
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(BodyError::Inner(error)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

/// The body type forwarded to upstreams.
pub type ProxyBody = Limited;

/// Buffers a body up to `max_bytes`, failing with `PayloadTooLarge` beyond it.
///
/// # Errors
/// Returns 413 `PayloadTooLarge` when the body exceeds `max_bytes` and 502
/// `DownstreamError` when the body cannot be read.
pub async fn collect<B>(body: B, max_bytes: u64) -> Result<Bytes, OagwError>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    let mut body = std::pin::pin!(body);
    let mut buffer: Vec<u8> = Vec::new();
    loop {
        let frame = futures_util::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        match frame {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    buffer.extend_from_slice(data);
                    if u64::try_from(buffer.len()).unwrap_or(u64::MAX) > max_bytes {
                        return Err(OagwError::new(
                            ErrorKind::PayloadTooLarge,
                            "response body exceeds the gateway limit",
                        ));
                    }
                }
            }
            Some(Err(error)) => {
                return Err(OagwError::new(
                    ErrorKind::DownstreamError,
                    format!("body could not be read: {error}"),
                ));
            }
            None => return Ok(Bytes::from(buffer)),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use hyper::body::Body as HttpBody;
    use std::task::Poll;

    #[test]
    fn body_error_renders_and_chains() {
        let error = BodyError::TooLarge;
        assert_eq!(error.to_string(), "request body exceeds the configured limit");
        assert!(std::error::Error::source(&error).is_none());
        let inner = BodyError::Inner(axum::Error::new(std::io::Error::other("boom")));
        assert!(std::error::Error::source(&inner).is_some());
    }

    #[test]
    fn limited_body_flags_oversized_payloads() {
        let body = axum::body::Body::from("0123456789");
        let mut limited = Limited::new(body, 4);
        let mut pinned = Pin::new(&mut limited);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let first = HttpBody::poll_frame(pinned.as_mut(), &mut cx);
        assert!(matches!(first, Poll::Ready(Some(Err(BodyError::TooLarge)))));
        let second = HttpBody::poll_frame(pinned.as_mut(), &mut cx);
        assert!(matches!(second, Poll::Ready(Some(Err(BodyError::TooLarge)))));
        assert!(limited.over_limit());

        let mut within = Limited::new(axum::body::Body::from("abc"), 4);
        let frame = HttpBody::poll_frame(Pin::new(&mut within), &mut cx);
        assert!(matches!(frame, Poll::Ready(Some(Ok(_)))));
        assert!(!within.over_limit());
    }
}
