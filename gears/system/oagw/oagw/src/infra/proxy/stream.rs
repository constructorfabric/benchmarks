//! Streaming response relay
//! (`cpt-cf-oagw-algo-request-proxy-stream-lifecycle`,
//! `cpt-cf-oagw-flow-request-proxy-sse-streaming`).
//!
//! The upstream response body is relayed *as it arrives*: no buffer holds the
//! conversation, the upstream close closes the client, and a mid-stream error
//! surfaces as the aborted-stream domain error so the
//! `X-OAGW-Error-Source` header of the response head names the side that
//! failed. The lifecycle events are recorded on the handle the response
//! carries, which is the pipeline-boundary outcome entry 2.9 consumes.
// @cpt-state:cpt-cf-oagw-state-request-proxy-stream-connection:p1

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::HttpBody;
use bytes::Bytes;
use futures_util::Stream;
use toolkit_http::{HttpError, LimitedBody};
use tokio::time::{Instant as TokioInstant, Sleep};

use crate::domain::error::DomainError;
use crate::domain::proxy::{ProxyByteStream, StreamEvent, StreamLifecycle};

// @cpt-begin:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-1
// @cpt-begin:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-2
// @cpt-begin:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-3
// @cpt-begin:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-4
// @cpt-begin:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-5
// @cpt-begin:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-6
/// The idle window this feature applies to an open streaming session.
///
/// `proxy_timeout_secs` bounds connection establishment and the complete
/// buffered exchange only; an open session is bounded by this idle window, so
//
// @cpt-end:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-6
// @cpt-end:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-5
// @cpt-end:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-4
// @cpt-end:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-3
// @cpt-end:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-2
// @cpt-end:cpt-cf-oagw-state-request-proxy-stream-connection:p1:inst-rp-st-conn-1
//
/// a session that keeps producing is never terminated by the proxy timeout.
/// The window is fixed: `proxy_timeout_secs` is not consulted here, because
/// the timeout it names applies to the buffered exchange.
pub const IDLE_WINDOW: Duration = Duration::from_secs(60);

/// The domain error a mid-stream abort is attributed with.
#[must_use]
pub fn abort_error(trace_id: Option<String>) -> DomainError {
    DomainError::StreamAborted { upstream_id: None, host: None, path: None, trace_id }
}

/// The domain error an idle window expiry is attributed with.
#[must_use]
pub fn idle_error(trace_id: Option<String>) -> DomainError {
    DomainError::IdleTimeout {
        upstream_id: None,
        host: None,
        guidance_secs: None,
        trace_id,
    }
}

/// Relay an upstream body into a client stream, recording the lifecycle.
///
/// The stream opens on first poll — the moment the first event is available —
/// and closes when the upstream closes. An upstream error yields the aborted
/// outcome once and then ends the stream, because the status of the response
/// head can no longer change.
#[must_use]
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-1
// `inst-rp-al-stream-1` .. `-9`, `inst-rp-sse-1` .. `-10`, `inst-rp-al-timeout-1`
// .. `-8`: the relay — the session opens on the first frame, the idle window is
// re-armed by every frame, and the close, the abort and the idle expiry are
// recorded on the lifecycle the response carries.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-6
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-8
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-9
pub fn relay(
    body: LimitedBody,
    lifecycle: Arc<StreamLifecycle>,
    idle: Duration,
    trace_id: Option<String>,
) -> ProxyByteStream {
    Box::pin(RelayStream {
        body,
        lifecycle,
        idle,
        idle_timer: Box::pin(tokio::time::sleep_until(TokioInstant::now() + idle)),
        opened: false,
        finished: false,
        trace_id,
    })
}
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-9
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-2
//

struct RelayStream {
    body: LimitedBody,
    lifecycle: Arc<StreamLifecycle>,
    idle: Duration,
    /// The armed idle timer: it is re-armed every time a frame arrives.
    idle_timer: Pin<Box<Sleep>>,
    /// Whether the stream already opened, so `Open` is recorded once.
    opened: bool,
    /// Whether the stream already ended in an abort, so nothing further is
    /// recorded and the stream simply ends.
    finished: bool,
    trace_id: Option<String>,
}

impl Stream for RelayStream {
    type Item = Result<Bytes, DomainError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        // The idle window is the only bound an open session carries. It is
        // armed at the first poll and re-armed by every frame that arrives.
        if this.idle_timer.as_mut().poll(cx).is_ready() {
            this.finished = true;
            this.lifecycle.record(StreamEvent::Aborted);
            return Poll::Ready(Some(Err(crate::infra::proxy::stream::idle_error(
                this.trace_id.clone(),
            ))));
        }
        match Pin::new(&mut this.body).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                match frame.data_ref() {
                    Some(data) => {
                        if !this.opened {
                            this.opened = true;
                            this.lifecycle.record(StreamEvent::Open);
                        }
                        this.idle_timer
                            .as_mut()
                            .reset(TokioInstant::now() + this.idle);
                        Poll::Ready(Some(Ok(data.clone())))
                    }
                    // A trailer frame carries no body bytes; keep polling.
                    None => {
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    }
                }
            }
            Poll::Ready(Some(Err(error))) => {
                this.finished = true;
                this.lifecycle.record(StreamEvent::Aborted);
                Poll::Ready(Some(Err(map_stream_error(&error, this.trace_id.clone()))))
            }
            Poll::Ready(None) => {
                this.finished = true;
                this.lifecycle.record(StreamEvent::Close);
                Poll::Ready(None)
            }
        }
    }
}

/// Map a transport error of a streaming body onto the domain error it is
/// attributed with.
#[must_use]
// @cpt-end:cpt-cf-oagw-algo-request-proxy-stream-lifecycle:p1:inst-rp-al-stream-1
pub fn map_stream_error(error: &HttpError, trace_id: Option<String>) -> DomainError {
    match error {
        HttpError::Timeout(_) | HttpError::DeadlineExceeded(_) => DomainError::IdleTimeout {
            upstream_id: None,
            host: None,
            guidance_secs: None,
            trace_id,
        },
        _ => abort_error(trace_id),
    }
}

/// Read a body to its end, returning the bytes.
///
/// The buffered leg is bounded: `max_bytes` is the ceiling the accumulated
/// buffer may never exceed, so an upstream response of arbitrary size cannot
/// drive the data plane into unbounded memory
/// (`cpt-cf-oagw-constraint-body-limit` applies to the response leg too, not
/// only to the request leg). A breach is reported as a downstream error, which
/// the caller enriches with the identifying fields.
///
/// # Errors
///
/// Returns the domain error the body's own failure maps onto, or a downstream
/// error when the ceiling is breached.
pub async fn collect(
    mut body: LimitedBody,
    max_bytes: usize,
    trace_id: Option<String>,
) -> Result<Bytes, DomainError> {
    let mut buffer = bytes::BytesMut::new();
    loop {
        let frame = match futures_util::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {
            Some(frame) => frame,
            None => break,
        };
        let frame = frame.map_err(|error| map_stream_error(&error, trace_id.clone()))?;
        if let Some(data) = frame.data_ref() {
            if buffer.len().saturating_add(data.len()) > max_bytes {
                return Err(DomainError::DownstreamError {
                    upstream_id: None,
                    host: None,
                    path: None,
                    trace_id,
                    retriable: false,
                });
            }
            buffer.extend_from_slice(data);
        }
    }
    Ok(buffer.freeze())
}
