//! The body type carried between the client and the upstream.
//!
//! Bodies are relayed rather than accumulated, so a server-sent-event stream
//! reaches the client incrementally and a large upload is never buffered whole.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};

use crate::domain::error::DomainError;

/// The hard body cap: 100 MB.
pub const MAX_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// A streaming body that can carry either direction of a proxied exchange.
///
/// Deliberately a concrete enum rather than a boxed trait object: hyper's
/// client requires the request body to be `Send`, and a boxed `axum::body::Body`
/// is `Send` but not `Sync`, which the boxed combinator would demand.
///
/// Every variant that carries an unbounded stream counts the bytes it has
/// relayed and fails the body once the 100 MB cap is crossed. A declared
/// over-cap length is refused before any byte moves; an undeclared (chunked)
/// body can only be cut off mid-stream, because the status line and headers
/// have already gone out by the time the overage is observable.
pub enum ProxyBody {
    /// No body.
    Empty,
    /// Exactly these bytes.
    Full(http_body_util::Full<Bytes>),
    /// An inbound client body, relayed as it arrives and capped.
    Client(axum::body::Body, Counter),
    /// An upstream response body, relayed as it arrives and capped.
    Upstream(Incoming, Counter),
}

/// Running byte total for one direction of an exchange.
#[derive(Debug, Default)]
pub struct Counter {
    seen: u64,
}

impl Counter {
    /// Add `n` bytes and report whether the cap has now been exceeded.
    fn add(&mut self, n: u64) -> bool {
        self.seen = self.seen.saturating_add(n);
        self.seen > MAX_BODY_BYTES
    }

    /// Bytes relayed so far.
    #[must_use]
    pub const fn seen(&self) -> u64 {
        self.seen
    }
}

impl ProxyBody {
    /// An empty body.
    #[must_use]
    pub const fn empty() -> Self {
        Self::Empty
    }

    /// A body holding exactly these bytes.
    #[must_use]
    pub fn from_bytes(b: Bytes) -> Self {
        Self::Full(http_body_util::Full::new(b))
    }

    /// Wrap an inbound axum body, relaying it as it arrives.
    #[must_use]
    pub fn from_axum(body: axum::body::Body) -> Self {
        Self::Client(body, Counter::default())
    }

    /// Wrap an upstream response body, relaying it as it arrives.
    #[must_use]
    pub fn from_incoming(body: Incoming) -> Self {
        Self::Upstream(body, Counter::default())
    }

    /// Convert into an axum body for the response path.
    pub fn into_axum(self) -> axum::body::Body {
        axum::body::Body::new(self)
    }
}

fn relay_err(what: &str, e: impl std::fmt::Display) -> DomainError {
    DomainError::UpstreamUnreachable {
        message: format!("{what} body error: {e}"),
    }
}

impl Body for ProxyBody {
    type Data = Bytes;
    type Error = DomainError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        /// Count a data frame and fail the body once the cap is crossed.
        fn capped(
            polled: Poll<Option<Result<Frame<Bytes>, DomainError>>>,
            counter: &mut Counter,
        ) -> Poll<Option<Result<Frame<Bytes>, DomainError>>> {
            match polled {
                Poll::Ready(Some(Ok(frame))) => {
                    let n = frame.data_ref().map_or(0, |d| d.len() as u64);
                    if counter.add(n) {
                        Poll::Ready(Some(Err(DomainError::PayloadTooLarge)))
                    } else {
                        Poll::Ready(Some(Ok(frame)))
                    }
                }
                other => other,
            }
        }

        match self.get_mut() {
            Self::Empty => Poll::Ready(None),
            Self::Full(f) => Pin::new(f)
                .poll_frame(cx)
                .map(|o| o.map(|r| r.map_err(|e| relay_err("client", e)))),
            Self::Client(b, c) => capped(
                Pin::new(b)
                    .poll_frame(cx)
                    .map(|o| o.map(|r| r.map_err(|e| relay_err("client", e)))),
                c,
            ),
            Self::Upstream(i, c) => capped(
                Pin::new(i)
                    .poll_frame(cx)
                    .map(|o| o.map(|r| r.map_err(|e| relay_err("upstream", e)))),
                c,
            ),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Empty => true,
            Self::Full(f) => f.is_end_stream(),
            Self::Client(b, _) => b.is_end_stream(),
            Self::Upstream(i, _) => i.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Empty => SizeHint::with_exact(0),
            Self::Full(f) => f.size_hint(),
            Self::Client(b, _) => b.size_hint(),
            Self::Upstream(i, _) => i.size_hint(),
        }
    }
}

/// Whether a declared content length exceeds the hard cap.
///
/// A declared over-cap length is refused before any byte is relayed. An
/// undeclared (chunked) body that turns out to exceed the cap can only be cut
/// off mid-stream, because the status and headers have already gone out.
#[must_use]
pub fn declared_length_exceeds_cap(headers: &hyper::HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_BODY_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;
    use hyper::header::{CONTENT_LENGTH, HeaderMap, HeaderValue};

    #[test]
    fn the_cap_is_one_hundred_megabytes() {
        assert_eq!(MAX_BODY_BYTES, 104_857_600);
    }

    #[test]
    fn a_declared_over_cap_length_is_detected() {
        let mut h = HeaderMap::new();
        h.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&(MAX_BODY_BYTES + 1).to_string()).unwrap(),
        );
        assert!(declared_length_exceeds_cap(&h));
    }

    #[test]
    fn a_declared_at_cap_length_is_permitted() {
        let mut h = HeaderMap::new();
        h.insert(
            CONTENT_LENGTH,
            HeaderValue::from_str(&MAX_BODY_BYTES.to_string()).unwrap(),
        );
        assert!(!declared_length_exceeds_cap(&h));
    }

    #[test]
    fn an_absent_or_unparsable_length_is_not_a_declared_overage() {
        assert!(!declared_length_exceeds_cap(&HeaderMap::new()));
        let mut h = HeaderMap::new();
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("not-a-number"));
        assert!(!declared_length_exceeds_cap(&h));
    }

    #[tokio::test]
    async fn a_byte_body_round_trips() {
        let b = ProxyBody::from_bytes(Bytes::from_static(b"hello"));
        let collected = b.collect().await.unwrap().to_bytes();
        assert_eq!(&collected[..], b"hello");
    }

    #[tokio::test]
    async fn an_empty_body_yields_no_bytes() {
        let collected = ProxyBody::empty().collect().await.unwrap().to_bytes();
        assert!(collected.is_empty());
    }
}
