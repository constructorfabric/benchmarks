//! HTTP forwarding through hyper-util's legacy client.
//!
//! The client streams request and response bodies (axum `Body`), so SSE /
//! chunked responses pass through without buffering. A total-request timeout
//! is applied on top of hyper (the legacy client has no request timeout), and
//! the response body stream gets an idle timeout so a stalled upstream cannot
//! hold a downstream connection open forever.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::Stream;
use http::{Request, Response};
use http_body_util::BodyExt;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::{Client, Error as LegacyError};
use hyper_util::rt::TokioExecutor;

/// Forwarder errors.
#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    /// The request exceeded `proxy_timeout_secs`.
    #[error("upstream request timed out: {detail}")]
    Timeout { detail: String },
    /// Connection / transport failure against the upstream.
    #[error("upstream connection failed: {detail}")]
    Connect { detail: String },
    /// The upstream closed the connection mid-stream.
    #[error("upstream stream aborted: {detail}")]
    Stream { detail: String },
    /// Request body exceeded `max_body_bytes`.
    #[error("request payload too large: {detail}")]
    PayloadTooLarge { detail: String },
}

/// HTTP forwarder (plain-text; TLS is terminated by the platform).
pub struct Forwarder {
    client: Client<HttpConnector, axum::body::Body>,
    timeout: Duration,
    max_body_bytes: usize,
}

impl Default for Forwarder {
    fn default() -> Self {
        Self::new(Duration::from_secs(2), 100 * 1024 * 1024)
    }
}

impl Forwarder {
    /// Build a forwarder with the given total-request timeout and request
    /// body limit. The legacy client builder is infallible, so no `Result`
    /// is returned.
    #[must_use]
    pub fn new(timeout: Duration, max_body_bytes: usize) -> Self {
        let client = Client::builder(TokioExecutor::new())
            .build::<_, axum::body::Body>(HttpConnector::new());
        Self {
            client,
            timeout,
            max_body_bytes,
        }
    }

    /// Forward a request and stream the response.
    ///
    /// # Errors
    ///
    /// Returns [`ForwardError::Timeout`] when the upstream does not respond
    /// within the configured timeout, or [`ForwardError::Connect`] on a
    /// connection / transport failure against the upstream.
    pub async fn send(
        &self,
        request: Request<axum::body::Body>,
    ) -> Result<Response<axum::body::Body>, ForwardError> {
        let fut = self.client.request(request);
        let response = tokio::time::timeout(self.timeout, fut)
            .await
            .map_err(|_| ForwardError::Timeout {
                detail: format!(
                    "upstream did not respond within {}s",
                    self.timeout.as_secs()
                ),
            })?
            .map_err(|e| ForwardError::Connect {
                detail: classify_connect_error(&e),
            })?;
        // Apply the idle timeout to the response body stream (the legacy
        // client stops enforcing any timeout once headers are received).
        let (parts, body) = response.into_parts();
        let stream = body
            .into_data_stream()
            .map(|r| r.map_err(Into::into))
            .boxed();
        let stream = IdleTimeoutStream::new(stream, self.timeout);
        Ok(http::Response::from_parts(
            parts,
            axum::body::Body::from_stream(stream),
        ))
    }

    /// Buffer a request body, enforcing the configured limit.
    ///
    /// # Errors
    ///
    /// Returns [`ForwardError::PayloadTooLarge`] when the body exceeds
    /// `max_body_bytes`, or [`ForwardError::Stream`] when the body stream
    /// aborts before it is fully buffered.
    pub async fn buffer_request_body(
        &self,
        body: axum::body::Body,
    ) -> Result<bytes::Bytes, ForwardError> {
        match axum::body::to_bytes(body, self.max_body_bytes).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if super::is_body_overflow(&e) => Err(ForwardError::PayloadTooLarge {
                detail: format!("request body exceeds {} bytes", self.max_body_bytes),
            }),
            Err(e) => Err(ForwardError::Stream {
                detail: format!("failed to buffer request body: {e}"),
            }),
        }
    }
}

fn classify_connect_error(e: &LegacyError) -> String {
    if e.is_connect() {
        format!("unable to connect to upstream: {e}")
    } else {
        format!("upstream request failed: {e}")
    }
}

/// A response-body stream that errors out when no chunk arrives within an
/// idle window — mirrors the total-request timeout for the body phase.
struct IdleTimeoutStream<S> {
    inner: S,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl<S> IdleTimeoutStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, axum::BoxError>> + Unpin,
{
    fn new(inner: S, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
        }
    }
}

impl<S> Stream for IdleTimeoutStream<S>
where
    S: Stream<Item = Result<bytes::Bytes, axum::BoxError>> + Unpin,
{
    type Item = Result<bytes::Bytes, axum::BoxError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(item)) => {
                // A chunk arrived — restart the idle window for the next one.
                let deadline = Instant::now() + self.idle;
                self.sleep.as_mut().reset(deadline.into());
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                if self.sleep.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Some(Err(Box::new(ForwardError::Stream {
                        detail: format!(
                            "upstream response body idle for over {}s",
                            self.idle.as_secs()
                        ),
                    }))));
                }
                Poll::Pending
            }
        }
    }
}
