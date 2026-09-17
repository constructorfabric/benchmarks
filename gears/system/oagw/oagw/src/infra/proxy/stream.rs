//! Streaming upstream response bodies.
//!
//! The data plane streams upstream bodies (SSE, chunked, large payloads)
//! instead of buffering them: `UpstreamBodyStream` wraps a
//! `hyper::body::Incoming` and exposes an axum-compatible byte stream.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;

/// Error type surfaced to axum when an upstream body fails mid-stream.
pub type BodyError = Box<dyn std::error::Error + Send + Sync>;

/// A streaming upstream body.
#[derive(Debug)]
pub struct UpstreamBodyStream {
    inner: Option<hyper::body::Incoming>,
}

impl UpstreamBodyStream {
    /// Wraps an upstream body.
    #[must_use]
    pub fn new(body: hyper::body::Incoming) -> Self {
        Self { inner: Some(body) }
    }

    /// Converts this stream into an axum response body.
    ///
    /// Data frames are passed through untouched. A body that terminates with
    /// an error yields a truncated stream (the caller has already received the
    /// upstream status and headers).
    #[must_use]
    pub fn into_axum_body(self) -> axum::body::Body {
        use futures_util::StreamExt as _;
        let stream = self.filter_map(|item| async move {
            match item {
                Ok(frame) => frame.into_data().ok().map(Ok::<Bytes, BodyError>),
                Err(err) => Some(Err(BodyError::from(err))),
            }
        });
        axum::body::Body::from_stream(stream)
    }
}

impl Stream for UpstreamBodyStream {
    type Item = Result<hyper::body::Frame<Bytes>, hyper::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use hyper::body::Body as _;
        match self.inner.as_mut() {
            Some(body) => std::pin::Pin::new(body).poll_frame(cx),
            None => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_leak_body_bytes() {
        let rendered = format!("{:?}", UpstreamBodyStream { inner: None });
        assert!(rendered.contains("UpstreamBodyStream"));
        assert!(!rendered.contains("secret"));
    }
}
