//! Streamed exchange of the OAGW gear (entry 2.6).
//!
//! The module is the handoff target of the proxy engine: the open upstream
//! exchange the entry-2.4 pipeline classified as streamed
//! (`OutcomeKind::Streamed`) and the upgrade request it handed over before its
//! own upstream call (`OutcomeKind::Upgrade`) are relayed here, one entry point
//! per outcome kind, and every failure is mapped onto the canonical error table
//! the entry-2.4 call classifier already fixed.
//!
//! The artifacts the FEATURE declares and this module implements:
//!
//! | Artifact | Implemented by |
//! |---|---|
//! | `cpt-cf-oagw-algo-sse-forward` | [`SseClassifier`], [`SseRelay`], [`streamed`] |
//! | `cpt-cf-oagw-algo-ws-upgrade` | [`validate_upgrade_headers`], [`inject_upgrade_headers`], [`upgraded`] |
//! | `cpt-cf-oagw-algo-ws-relay` | [`relay_frames`], [`HyperIo`], [`Monitored`] |
//! | `cpt-cf-oagw-algo-stream-lifecycle` | [`open_record`] and the calls that advance it |
//! | `cpt-cf-oagw-algo-stream-error-classify` | [`SseRelay::classify`], the relay tear-downs |
//! | `cpt-cf-oagw-algo-stream-timeout` | the idle window of both relays |
//!
//! The relay holds no more than one chunk in flight, arms one idle window per
//! exchange and resets it on every byte in either direction, and records the
//! close reason, the failing direction, the byte counts and the error type on
//! the [`StreamRecord`](crate::domain::stream::StreamRecord) the request context
//! carries for entry 2.7. No credential material, no header value, no request
//! body byte and no query string reaches that record.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes, HttpBody as _};
use axum::response::Response;
use futures_util::Stream;
use http::header::{ACCEPT, CONNECTION, CONTENT_TYPE, UPGRADE};
use http::{HeaderMap, HeaderValue, StatusCode};
use hyper::rt::{Read as HyperRead, Write as HyperWrite};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::domain::error::DomainError;
use crate::domain::model::Endpoint;
use crate::domain::stream::{
    CloseReason, SharedRecord, StreamErrorClassifier, StreamErrorDecision, StreamEvent,
    StreamFailure, StreamKind, StreamRecord, StreamState, WsEvent, WsState, idle_window,
};
use crate::infra::proxy::call::{OutboundRequest, UpstreamCaller};
use crate::infra::proxy::context::RequestContext;
use crate::infra::proxy::engine::ProxyOutcome;
use crate::infra::proxy::passthrough::{ERROR_SOURCE_UPSTREAM, EVENT_STREAM};

/// The error type an upstream body yields, as the entry-2.1 mapping layer types it.
type BodyError = axum::BoxError;

/// The `Sec-WebSocket-Version` this gateway forwards (`inst-ss-upg-06`).
const SUPPORTED_WS_VERSION: &str = "13";

/// The value `Upgrade` names for the protocol the upgrade path carries.
const WEBSOCKET_UPGRADE: &str = "websocket";

/// The header names an upgrade re-injects after the entry-2.4 transformation.
const WS_KEY: &str = "sec-websocket-key";
const WS_VERSION: &str = "sec-websocket-version";
const WS_PROTOCOL: &str = "sec-websocket-protocol";
const WS_EXTENSIONS: &str = "sec-websocket-extensions";

/// The header the gateway never injects or computes (`inst-ss-upg-09`).
const WS_ACCEPT: &str = "sec-websocket-accept";

/// What a streamed exchange hands back to the transport.
///
/// The transport owns the entry-2.1 mapping layer, so a gateway failure is
/// returned as the mapped [`DomainError`] together with the request context the
/// failure closed, and the transport builds the `application/problem+json`
/// document from both and stamps `X-OAGW-Error-Source: gateway` on it. A
/// response that came from the upstream is returned with the source the handoff
/// already recorded, so the transport stamps `X-OAGW-Error-Source: upstream`
/// without rewriting a head the upstream owns.
#[derive(Debug)]
pub enum StreamReply {
    /// The response to write to the client connection, with its error source and
    /// the request context the relay closed: the context carries the
    /// [`StreamRecord`](crate::domain::stream::StreamRecord) the relay advanced,
    /// so the byte counts, the close reason and the error type of the exchange
    /// survive the handoff and reach entry 2.7.
    Response(Response, &'static str, Box<RequestContext>),
    /// A gateway failure the transport maps onto a canonical problem document.
    ///
    /// The context is boxed because it is several times the size of a
    /// response head: the reply is built once per request and moved, so the
    /// indirection keeps the variant the transport matches on small.
    Failed(DomainError, Box<RequestContext>),
}

/// The two independent facts of an exchange (`inst-ss-fwd-01`).
///
/// A streamed **response** is declared by a `Content-Type` of
/// `text/event-stream`; a streamed **request** by an `Accept` header that names
/// it. The two facts are read independently: a request that asked for a stream
/// whose response is not one stays on the entry-2.4 buffered path, and a
/// response that streams for a client that never asked is still streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SseFacts {
    /// The request declared `Accept: text/event-stream`.
    pub request: bool,
    /// The response declared `Content-Type: text/event-stream`.
    pub response: bool,
}

impl SseFacts {
    /// Whether the response body is the streamed body the feature forwards.
    #[must_use]
    pub const fn is_stream(&self) -> bool {
        self.response
    }

    /// The stream kind the facts declare, when any of them does.
    ///
    /// The response fact wins: the body is what the gateway forwards.
    #[must_use]
    pub const fn kind(&self) -> Option<StreamKind> {
        if self.response {
            Some(StreamKind::SseResponse)
        } else if self.request {
            Some(StreamKind::SseRequest)
        } else {
            None
        }
    }
}

/// The media-type classification of one exchange
/// (`cpt-cf-oagw-algo-sse-forward`).
///
/// Each comparison reads the media type of one header value, ignores every
/// parameter after the `;` and compares case-insensitively, and reads the two
/// headers independently of each other.
#[derive(Debug, Clone, Copy, Default)]
pub struct SseClassifier;

impl SseClassifier {
    /// Classify one exchange from its request `Accept` and response
    /// `Content-Type` values, either of which may be absent.
    #[must_use]
    pub fn classify(accept: Option<&str>, content_type: Option<&str>) -> SseFacts {
        SseFacts {
            request: names_event_stream(accept),
            response: names_event_stream(content_type),
        }
    }
}

/// Whether one header value names the streamed media type, ignoring parameters.
fn names_event_stream(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value
            .split(';')
            .next()
            .unwrap_or(value)
            .trim()
            .eq_ignore_ascii_case(EVENT_STREAM)
    })
}

/// Open the in-memory record of a handed-off exchange
/// (`cpt-cf-oagw-algo-stream-lifecycle`).
///
/// The record carries the stream kind, the identity of the selected endpoint —
/// the authority the pipeline dialed, never a client-supplied host — and the
/// idle window read from the same `oagw.config.proxy_timeout_secs` value the
/// entry-2.4 call used (`inst-ss-tmo-01`).
#[must_use]
fn open_record(kind: StreamKind, endpoint: Option<&Endpoint>, timeout: Duration) -> SharedRecord {
    // @cpt-begin:cpt-cf-oagw-algo-stream-lifecycle:p1:inst-ss-lif-01
    let identity = endpoint.map(|endpoint| format!("{}:{}", endpoint.host, endpoint.port));
    Arc::new(StreamRecord::new(kind, identity, idle_window(timeout)))
    // @cpt-end:cpt-cf-oagw-algo-stream-lifecycle:p1:inst-ss-lif-01
}

/// The gateway failure a streamed exchange returns to the transport.
///
/// A head that was never committed can still carry a canonical problem
/// document, so the relay answers with the mapped row and the transport stamps
/// the gateway source on it (`inst-ss-err-04`).
fn gateway_problem(error: DomainError, context: RequestContext) -> StreamReply {
    StreamReply::Failed(error, Box::new(context))
}

/// The emission the transport built for one exchange's close, run with the
/// request context the relay holds.
type ExchangeEmission = Box<dyn FnOnce(&RequestContext) + Send>;

/// The close a handed-off exchange still owes, owned by the relay that ends it.
///
/// The transport builds the emission the exchange closes with and hands it over
/// at the handoff; the relay runs it exactly once, at the transition that puts
/// the record on its terminal state, and releases the in-flight accounting the
/// exchange opened with it. A relay that is dropped before that — a client that
/// went away while the body was still streaming — runs it from the drop, so the
/// exchange is closed on that exit too, and never twice.
pub struct ExchangeClose {
    /// The emission the transport built, run with the context the relay holds.
    emit: Option<ExchangeEmission>,
    /// The context the emission reads, bound when the relay took the close.
    context: Option<RequestContext>,
    /// The in-flight accounting the exchange opened, released with the close.
    in_flight: Option<crate::infra::obs::metrics::InFlightGuard>,
}

impl std::fmt::Debug for ExchangeClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExchangeClose").finish_non_exhaustive()
    }
}

impl ExchangeClose {
    /// A close that runs `emit` with the request context the relay binds to it.
    #[must_use]
    pub(crate) fn new(emit: ExchangeEmission) -> Self {
        Self {
            emit: Some(emit),
            context: None,
            in_flight: None,
        }
    }

    /// Bind the context the emission reads.
    ///
    /// The record the relay advances is the one the context carries, so the
    /// emission reads the outcome the relay is still writing and not a copy
    /// taken before the relay ran.
    fn bind(&mut self, context: &RequestContext) {
        self.context = Some(context.clone());
    }

    /// Take the in-flight accounting the exchange opened, released with the
    /// close and not with the handoff.
    fn hold(&mut self, in_flight: Option<crate::infra::obs::metrics::InFlightGuard>) {
        self.in_flight = in_flight;
    }

    /// Run the emission and release the accounting, exactly once.
    fn finish(&mut self) {
        if let Some(context) = self.context.take()
            && let Some(emit) = self.emit.take()
        {
            emit(&context);
        }
        self.in_flight.take();
    }
}

impl Drop for ExchangeClose {
    fn drop(&mut self) {
        // The close is owed whether the relay reached its terminal transition or
        // was dropped on the way to it, so the drop runs it.
        self.finish();
    }
}

// @cpt-begin:cpt-cf-oagw-dod-sse-forwarding:p1:inst-full
/// The relay of one streamed response body
/// (`cpt-cf-oagw-algo-sse-forward`).
///
/// The relay reads one frame at a time from the unread body the handoff left,
/// yields it to the client connection in the order received, resets the idle
/// window on every chunk and counts the bytes it forwards. No fabricated byte is
/// spliced into the stream and no buffered response is rebuilt.
struct SseRelay {
    /// The unread upstream body the handoff left.
    body: toolkit_http::ResponseBody,
    /// The idle window armed for the exchange (`inst-ss-tmo-01`).
    idle: Duration,
    /// The window, armed once and reset on every chunk, so a poll that yields
    /// nothing cannot extend the silence the window measures (`inst-ss-tmo-02`).
    window: Pin<Box<tokio::time::Sleep>>,
    /// The record the lifecycle and the byte counts are advanced on.
    record: SharedRecord,
    /// Bytes handed to the client connection so far (`inst-ss-sse-11`).
    relayed: u64,
    /// Whether the response head has been committed to the client
    /// (`inst-ss-cls-01`).
    head_committed: bool,
    /// The chunk read before the head was relayed, still in flight.
    in_flight: Option<Bytes>,
    /// The decision of the failure that stopped the relay, when it stopped.
    stop: Option<StreamErrorDecision>,
    /// Whether the exchange still needs its terminal event.
    open: bool,
    /// The close the exchange owes, run at the terminal transition.
    close: Option<ExchangeClose>,
}

impl SseRelay {
    /// A relay over the unread body of a handed-off exchange.
    fn new(body: toolkit_http::ResponseBody, record: SharedRecord, idle: Duration) -> Self {
        Self {
            body,
            idle,
            window: Box::pin(tokio::time::sleep(idle)),
            record,
            relayed: 0,
            head_committed: false,
            in_flight: None,
            stop: None,
            open: true,
            close: None,
        }
    }

    /// Whether a body byte has been flushed to the client (`inst-ss-cls-01`).
    const fn body_flushed(&self) -> bool {
        self.relayed > 0
    }

    /// Classify a failure from the two facts that decide the response
    /// (`inst-ss-cls-01`, `inst-ss-cls-02`).
    fn classify(&self, failure: StreamFailure) -> StreamErrorDecision {
        StreamErrorClassifier::classify(failure, self.head_committed, self.body_flushed())
    }

    /// Take the close the exchange owes, to be run by this relay.
    ///
    /// A relay whose head was committed runs to its terminal state after the
    /// handoff, so the close the transport built moves onto it and runs exactly
    /// once, when the record reaches that state.
    fn arm(&mut self, close: ExchangeClose) {
        self.close = Some(close);
    }

    /// Run the close the terminal transition the record just took produced.
    fn closed(&mut self) {
        if let Some(close) = self.close.as_mut() {
            close.finish();
        }
    }

    /// Record the terminal outcome `decision` produced and stop the relay.
    ///
    /// The error type is recorded on the lifecycle (`inst-ss-cls-10`), the close
    /// reason and the failing direction with it, and the exchange is advanced
    /// onto its terminal state (`inst-ss-cls-08`).
    fn tear_down(&mut self, decision: StreamErrorDecision, error: bool) {
        if error {
            self.record.record_error(&decision.error);
        }
        let _ = self.record.advance(StreamEvent::Failed {
            side: decision.closing_side,
            reason: decision.reason,
        });
        self.stop = Some(decision);
        self.open = false;
        self.closed();
    }

    /// Record the idle teardown and the window that fired
    /// (`inst-ss-tmo-04`, `inst-ss-tmo-05`, `inst-ss-tmo-07`).
    fn timed_out(&mut self) {
        let decision = self.classify(StreamFailure::IdleElapsed);
        self.record.record_error(&decision.error);
        let _ = self.record.advance(StreamEvent::IdleElapsed);
        self.stop = Some(decision);
        self.open = false;
        self.closed();
    }

    /// Record the clean close of an exchange the upstream ended
    /// (`inst-ss-ucl-05`, `inst-ss-ucl-06`).
    fn ended(&mut self) {
        let _ = self.record.advance(StreamEvent::UpstreamEnded);
        self.open = false;
        self.closed();
    }

    /// Read one frame from the upstream under the idle window.
    ///
    /// The wait for the next frame is bounded by the armed window, which is
    /// reset on every chunk and never on a poll that yields nothing, so a stream
    /// that keeps producing is never torn down by this path however long it runs
    /// (`inst-ss-tmo-02`, `inst-ss-tmo-06`).
    fn poll_chunk(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, BodyError>>> {
        loop {
            if let Some(bytes) = self.in_flight.take() {
                return Poll::Ready(Some(Ok(bytes)));
            }
            if !self.open {
                return Poll::Ready(None);
            }
            match self.window.as_mut().poll(cx) {
                // The window elapsed with no byte in either direction
                // (`inst-ss-tmo-03`): the exchange is torn down and the body
                // ends, so a head that was committed on a live connection is
                // closed.
                Poll::Ready(()) => {
                    self.timed_out();
                    return Poll::Ready(None);
                }
                Poll::Pending => {}
            }
            match ready!(Pin::new(&mut self.body).poll_frame(cx)) {
                // The upstream ended the stream: the client connection is closed
                // cleanly after the tail that was already sent
                // (`inst-ss-fwd-13`, `inst-ss-ucl-01`).
                None => {
                    self.ended();
                    return Poll::Ready(None);
                }
                // The upstream aborted, reset or lost the exchange mid-body
                // (`inst-ss-fwd-11`): the failure is classified and the body
                // ends, so no fabricated byte is spliced into the stream
                // (`inst-ss-fwd-14`).
                Some(Err(error)) => {
                    let decision = self.classify(StreamFailure::UpstreamLoss);
                    self.tear_down(decision, true);
                    return Poll::Ready(Some(Err(error)));
                }
                Some(Ok(frame)) => match frame.into_data() {
                    // A data frame is forwarded unchanged, one chunk in flight,
                    // with the window reset and the byte count recorded
                    // (`inst-ss-fwd-04`, `inst-ss-fwd-05`, `inst-ss-fwd-07`).
                    Ok(bytes) => {
                        self.window
                            .as_mut()
                            .reset(tokio::time::Instant::now() + self.idle);
                        let size = bytes.len() as u64;
                        self.relayed += size;
                        self.record.count_downstream(size);
                        // The chunk is leaving for the client connection: the
                        // first one moves the exchange from open onto relaying
                        // (`inst-ss-stl-04`). The head is the pre-condition, so
                        // a chunk read before it was committed never raises it.
                        if self.head_committed && self.record.state() == StreamState::Open {
                            let _ = self.record.advance(StreamEvent::ChunkFlushed);
                        }
                        return Poll::Ready(Some(Ok(bytes)));
                    }
                    // A trailer frame carries no body byte: the upstream
                    // declared framing is passed through, so the frame is
                    // dropped and the loop reads the next frame — an upstream
                    // that sends an unbounded run of trailers cannot drive this
                    // through recursion.
                    Err(_) => continue,
                },
            }
        }
    }
}

impl Stream for SseRelay {
    type Item = Result<Bytes, BodyError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_chunk(cx)
    }
}

impl Drop for SseRelay {
    fn drop(&mut self) {
        // The body was dropped, so the client connection is gone: the upstream
        // connection is closed without draining its body, the in-flight chunk is
        // discarded, no response is written and the close is recorded
        // (`cpt-cf-oagw-flow-stream-disconnect`).
        if !self.open {
            return;
        }
        let decision = self.classify(StreamFailure::ClientDisconnect);
        self.tear_down(decision, false);
    }
}

/// The streamed half of a handed-off exchange (`OutcomeKind::Streamed`).
///
/// The head is relayed as received — no `Content-Length` is added,
/// `Content-Type` is not recomputed, the upstream's `Cache-Control` values and
/// declared framing are passed through — and the body is relayed one chunk at a
/// time. The head is committed only once the first chunk is known, so an
/// upstream that aborts before the first byte, or stays silent past the idle
/// window, is answered with the mapped problem document instead of an exchange
/// that can never produce a body (`inst-ss-fwd-12`, `inst-ss-idle-08`).
///
/// # Errors
///
/// Never returns an error: every failure is returned as the mapped
/// [`DomainError`] with the request context it closed.
pub async fn streamed(
    outcome: ProxyOutcome,
    inbound: &HeaderMap,
    timeout: Duration,
    mut close: ExchangeClose,
) -> StreamReply {
    let ProxyOutcome {
        body,
        headers,
        status,
        error_source,
        cors,
        endpoint,
        context,
        in_flight,
        ..
    } = outcome;
    let Some(body) = body else {
        return gateway_problem(
            DomainError::ProtocolError {
                detail: "the streamed handoff carries no unread body".to_owned(),
            },
            context,
        );
    };
    let mut context = context;

    // @cpt-begin:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-01
    // @cpt-begin:cpt-cf-oagw-flow-sse-proxy:p1:inst-ss-sse-07
    // The two facts are read from the request `Accept` and the response
    // `Content-Type` values, independently of each other, and the stream kind
    // they declare is what the record carries for entry 2.7.
    let facts = SseClassifier::classify(
        inbound.get(ACCEPT).and_then(|value| value.to_str().ok()),
        headers.get(CONTENT_TYPE).and_then(|value| value.to_str().ok()),
    );
    let kind = facts.kind().unwrap_or(StreamKind::SseResponse);
    let record = open_record(kind, endpoint.as_ref(), timeout);
    context.stream = Some(record.clone());
    // @cpt-end:cpt-cf-oagw-flow-sse-proxy:p1:inst-ss-sse-07
    // @cpt-end:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-01

    // @cpt-begin:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-03
    // The body the handoff carries is not re-read for a size limit: the relay
    // holds one chunk ahead of the client connection and nothing more, so a
    // buffered limit would measure the wrong thing on a stream that never ends.
    // @cpt-end:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-03

    let mut relay = SseRelay::new(body, record.clone(), timeout);
    // @cpt-begin:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-10
    // The first chunk decides the shape of the response: read with the idle
    // window armed and one chunk ahead at most.
    let first = std::future::poll_fn(|cx| relay.poll_chunk(cx)).await;
    // @cpt-end:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-10

    let head = head_of(status, &headers, &cors, context.cross_origin);
    match first {
        // @cpt-begin:cpt-cf-oagw-flow-sse-proxy:p1:inst-ss-sse-08
        // @cpt-begin:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-02
        // The head is relayed as received — the entry-2.1 header layer already
        // stamped `X-OAGW-Error-Source` on it — and the body is relayed from
        // here on, one chunk at a time, starting with the chunk in flight.
        Some(Ok(first)) => {
            relay.head_committed = true;
            relay.in_flight = Some(first);
            // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-04
            // The head was committed and the body is still streaming, so the
            // record reaches its terminal state after the handoff: the close the
            // transport built moves onto the relay, which holds the request
            // context and the in-flight accounting the exchange opened and runs
            // the close exactly once, at the transition that ends the exchange —
            // the upstream's end, an abort, the idle window, or the client that
            // went away while the body was streaming.
            close.bind(&context);
            close.hold(in_flight);
            relay.arm(close);
            // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-04
            let response = match head.body(Body::from_stream(relay)) {
                Ok(response) => response,
                Err(_) => {
                    return gateway_problem(
                        DomainError::ProtocolError {
                            detail: "the streamed response could not be relayed".to_owned(),
                        },
                        context,
                    );
                }
            };
            // @cpt-begin:cpt-cf-oagw-algo-stream-lifecycle:p1:inst-ss-lif-02
            // @cpt-begin:cpt-cf-oagw-state-stream-lifecycle:p1:inst-ss-stl-01
            // The head was relayed: the exchange is open. The first chunk the
            // transport takes from the body moves it onto relaying, which the
            // relay raises itself when that chunk is handed out
            // (`inst-ss-stl-04`).
            let _ = record.advance(StreamEvent::HeadRelayed);
            // @cpt-end:cpt-cf-oagw-state-stream-lifecycle:p1:inst-ss-stl-01
            // @cpt-end:cpt-cf-oagw-algo-stream-lifecycle:p1:inst-ss-lif-02
            // The context leaves with the reply, so the record the relay body
            // still advances reaches entry 2.7 when the exchange closes.
            StreamReply::Response(response, error_source, Box::new(context))
            // @cpt-end:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-02
            // @cpt-end:cpt-cf-oagw-flow-sse-proxy:p1:inst-ss-sse-08
        }
        // @cpt-begin:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-03
        // @cpt-begin:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-04
        // The upstream ended the streamed response without a body: the head is
        // relayed, the client connection is closed cleanly and no byte is
        // withheld (`inst-ss-sse-12`, `inst-ss-stl-05`).
        None if relay.stop.is_none() => {
            let response = match head.body(Body::empty()) {
                Ok(response) => response,
                Err(_) => {
                    return gateway_problem(
                        DomainError::ProtocolError {
                            detail: "the streamed response could not be relayed".to_owned(),
                        },
                        context,
                    );
                }
            };
            let _ = record.advance(StreamEvent::HeadRelayed);
            let _ = record.advance(StreamEvent::UpstreamEnded);
            StreamReply::Response(response, error_source, Box::new(context))
            // @cpt-end:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-04
            // @cpt-end:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-03
        }
        // @cpt-begin:cpt-cf-oagw-flow-stream-error:p1:inst-ss-err-03
        // @cpt-begin:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-11
        // The upstream aborted before the first body byte, or stayed silent past
        // the idle window: the head has not been committed and no body byte has
        // been flushed, so the mapped problem document is the response
        // (`inst-ss-fwd-12`, `inst-ss-idle-08`).
        Some(Err(_)) | None => {
            let error = relay.stop.as_ref().map_or_else(
                || DomainError::StreamAborted {
                    detail: "the upstream stream was lost while it was being relayed".to_owned(),
                },
                |decision| decision.error.clone(),
            );
            gateway_problem(error, context)
            // @cpt-end:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-11
            // @cpt-end:cpt-cf-oagw-flow-stream-error:p1:inst-ss-err-03
        }
    }
}

/// The response head of a handed-off exchange, as received.
///
/// No `Content-Length` is added, `Content-Type` is not recomputed, the
/// upstream's `Cache-Control` values are preserved verbatim and the upstream's
/// declared framing is forwarded (`inst-ss-fwd-02`); only the cross-origin
/// response headers the entry-2.2 policy promised are added on top.
fn head_of(
    status: StatusCode,
    headers: &HeaderMap,
    cors: &crate::infra::proxy::validate::CorsDecision,
    cross_origin: bool,
) -> http::response::Builder {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        builder = builder.header(name, value);
    }
    for (name, value) in
        crate::infra::proxy::validate::cors_response_headers(cors, cross_origin)
    {
        builder = builder.header(name, value);
    }
    builder
}
// @cpt-end:cpt-cf-oagw-dod-sse-forwarding:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-ws-upgrade:p1:inst-full
/// Whether `headers` carries a well-formed upgrade request
/// (`cpt-cf-oagw-algo-ws-upgrade`).
///
/// `Upgrade` names `websocket`, `Connection` names `Upgrade`,
/// `Sec-WebSocket-Key` is present and `Sec-WebSocket-Version` is the one version
/// this gateway forwards. The check runs before any upstream call.
///
/// # Errors
///
/// Returns the mapped `400` of a request that is not well formed.
pub fn validate_upgrade_headers(headers: &HeaderMap) -> Result<(), DomainError> {
    let upgrade = text_of(headers, UPGRADE).unwrap_or_default();
    if !names_token(upgrade, WEBSOCKET_UPGRADE) {
        return Err(malformed("the `Upgrade` header does not name `websocket`"));
    }
    let connection = text_of(headers, CONNECTION).unwrap_or_default();
    if !names_token(connection, "Upgrade") {
        return Err(malformed(
            "the `Connection` header does not name `Upgrade`",
        ));
    }
    if text_of(headers, WS_KEY).is_none_or(|key| key.trim().is_empty()) {
        return Err(malformed("the `Sec-WebSocket-Key` header is missing"));
    }
    if text_of(headers, WS_VERSION).unwrap_or_default().trim() != SUPPORTED_WS_VERSION {
        return Err(malformed(
            "the `Sec-WebSocket-Version` header is not one this gateway forwards",
        ));
    }
    Ok(())
}

/// Re-inject the validated upgrade headers onto the transformed set
/// (`inst-ss-upg-08`).
///
/// Exactly `Upgrade: websocket`, `Connection: Upgrade` and the client's
/// `Sec-WebSocket-Key`, `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and
/// `Sec-WebSocket-Extensions` where the client supplied them are written onto
/// the set the entry-2.4 transformation left, which stripped the hop-by-hop
/// pair. No `Sec-WebSocket-Accept` is injected: the peer that accepts the
/// upgrade computes it, and this gateway computes or validates it nowhere
/// (`inst-ss-upg-09`).
pub fn inject_upgrade_headers(outbound: &mut HeaderMap, client: &HeaderMap) {
    outbound.insert(UPGRADE, HeaderValue::from_static(WEBSOCKET_UPGRADE));
    outbound.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    for name in [WS_KEY, WS_VERSION, WS_PROTOCOL, WS_EXTENSIONS] {
        if let Some(value) = client.get(name) {
            outbound.insert(name, value.clone());
        }
    }
    outbound.remove(WS_ACCEPT);
}

/// Whether a comma-separated header value names `token`, case-insensitively.
fn names_token(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

/// The first text value of one header, when it is representable as text.
fn text_of(headers: &HeaderMap, name: impl http::header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// The `400` of an upgrade request that is not well formed.
fn malformed(detail: &str) -> DomainError {
    DomainError::ValidationError {
        detail: detail.to_owned(),
    }
}

/// The upgrade half of a handed-off exchange (`OutcomeKind::Upgrade`).
///
/// The request is validated before any upstream call, the scheme posture of the
/// constraint is applied with the recorded assumption-2 correction, the upgrade
/// headers are re-injected onto the transformed set and the selected endpoint is
/// dialed as exactly one request over the crate's existing client stack. A `101`
/// is relayed verbatim and its frames are relayed in both directions; a refusal
/// is passed through; a failed attempt is mapped onto the row the entry-2.4 call
/// classifier already fixed.
///
/// # Errors
///
/// Never returns an error: every failure is returned as the mapped
/// [`DomainError`] with the request context it closed.
pub async fn upgraded(
    outcome: ProxyOutcome,
    inbound: &HeaderMap,
    caller: &UpstreamCaller,
    client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    timeout: Duration,
    mut close: ExchangeClose,
) -> StreamReply {
    let ProxyOutcome {
        headers,
        cors,
        endpoint,
        target,
        context,
        in_flight,
        ..
    } = outcome;
    let Some(endpoint) = endpoint else {
        return gateway_problem(
            DomainError::ProtocolError {
                detail: "the upgrade handoff carries no selected endpoint".to_owned(),
            },
            context,
        );
    };
    let mut context = context;
    let record = open_record(StreamKind::WebSocket, Some(&endpoint), timeout);
    context.stream = Some(record.clone());

    // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-01
    // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-03
    // The upgrade request is validated before any upstream connection is opened,
    // so a request that is not well formed never reaches an upstream.
    if let Err(error) = validate_upgrade_headers(inbound) {
        let _ = record.advance_session(WsEvent::Refused);
        let _ = record.advance(StreamEvent::Failed {
            side: crate::domain::stream::SIDE_NONE,
            reason: CloseReason::Rejected,
        });
        record.record_error(&error);
        return gateway_problem(error, context);
    }
    // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-03
    // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-01

    // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-04
    // @cpt-begin:cpt-cf-oagw-dod-stream-scheme-posture:p1:inst-full
    // The scheme posture of the constraint, with the assumption-2 correction: a
    // `wss` endpoint carries the upgrade over TLS and an `http` endpoint is
    // admitted only when the operator opted in. The refusal is returned before
    // any connection attempt, from the same gate the entry-2.4 call runs.
    if let Err(error) = caller.enforce_posture(&endpoint) {
        let _ = record.advance_session(WsEvent::Refused);
        let _ = record.advance(StreamEvent::Failed {
            side: crate::domain::stream::SIDE_NONE,
            reason: CloseReason::Rejected,
        });
        record.record_error(&error);
        return gateway_problem(error, context);
    }
    // @cpt-end:cpt-cf-oagw-dod-stream-scheme-posture:p1:inst-full
    // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-04

    // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-08
    // The re-injection runs on the transformed set the handoff carried, so the
    // hop-by-hop pair the transformation stripped is restored and no further
    // hop-by-hop header is added.
    let mut outbound = headers.clone();
    inject_upgrade_headers(&mut outbound, inbound);
    // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-08

    // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-07
    // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-10
    // The selected endpoint is dialed over the crate's existing client stack:
    // TLS for `wss`, plaintext HTTP/1.1 for an admitted `http`, with the connect
    // target always the stored endpoint and never a client-supplied host, and
    // with the client request never re-issued as a second request.
    let request = OutboundRequest {
        method: http::Method::GET,
        url: target.unwrap_or_default(),
        headers: outbound
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        body: Bytes::new(),
        endpoint,
    };
    // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-10
    // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-07
    let mut raw = match caller.call_upgrade(request).await {
        Ok(raw) => raw,
        Err(error) => {
            // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-18
            // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-17
            // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-18
            // The attempt itself failed: the failure is mapped onto the row the
            // entry-2.4 call classifier already fixed for that cause, the session
            // is aborted and no relay is started.
            let _ = record.advance_session(WsEvent::AttemptFailed);
            let _ = record.advance(StreamEvent::Failed {
                side: crate::domain::stream::SIDE_UPSTREAM,
                reason: CloseReason::Aborted,
            });
            record.record_error(&error);
            return gateway_problem(error, context);
            // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-18
            // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-17
            // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-18
        }
    };

    // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-11
    if raw.status() == StatusCode::SWITCHING_PROTOCOLS {
        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-12
        // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-08
        // The `101` status line and its headers are relayed verbatim: the
        // upstream's `Sec-WebSocket-Accept`, any negotiated
        // `Sec-WebSocket-Protocol` and any negotiated extension are neither
        // added nor altered by this gateway.
        let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        for (name, value) in raw.headers().iter() {
            builder = builder.header(name, value);
        }
        for (name, value) in
            crate::infra::proxy::validate::cors_response_headers(&cors, context.cross_origin)
        {
            builder = builder.header(name, value);
        }
        let response = match builder.body(Body::empty()) {
            Ok(response) => response,
            Err(_) => {
                return gateway_problem(
                    DomainError::ProtocolError {
                        detail: "the negotiated upgrade could not be relayed".to_owned(),
                    },
                    context,
                );
            }
        };
        // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-08
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-12

        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-19
        // The negotiated facts are recorded as booleans on the request context:
        // no header value beyond that reaches the record.
        record.negotiated(
            raw.headers().contains_key(WS_PROTOCOL),
            raw.headers().contains_key(WS_EXTENSIONS),
        );
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-19

        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-13
        // The `101` head leaves with the source the upstream owns — the
        // entry-2.1 header layer stamps `X-OAGW-Error-Source` from the source the
        // reply carries — and the session is marked established.
        // @cpt-begin:cpt-cf-oagw-algo-stream-lifecycle:p1:inst-ss-lif-02
        // @cpt-begin:cpt-cf-oagw-state-ws-session:p1:inst-ss-stw-01
        // The `101` head was relayed to the client: the exchange is open and the
        // session is established.
        let _ = record.advance(StreamEvent::HeadRelayed);
        let _ = record.advance_session(WsEvent::Established);
        // @cpt-end:cpt-cf-oagw-state-ws-session:p1:inst-ss-stw-01
        // @cpt-end:cpt-cf-oagw-algo-stream-lifecycle:p1:inst-ss-lif-02
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-13

        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-14
        // @cpt-begin:cpt-cf-oagw-algo-ws-relay:p1:inst-ss-rlb-01
        // The established pair of connections is handed to the relay: the client
        // side is the handle the request extensions carried and the upstream
        // side is the one the `101` carries. The relay outlives the request, so
        // it runs on its own task and the response is returned now.
        let upstream_upgrade = hyper::upgrade::on(&mut raw);
        // The close the exchange owes runs when the session ends, not at the
        // handoff: the session outlives the request and holds the request
        // context and the in-flight accounting the exchange opened.
        close.bind(&context);
        close.hold(in_flight);
        tokio::spawn(relay_session(
            client_upgrade,
            upstream_upgrade,
            record.clone(),
            close,
            timeout,
        ));
        StreamReply::Response(response, ERROR_SOURCE_UPSTREAM, Box::new(context))
        // @cpt-end:cpt-cf-oagw-algo-ws-relay:p1:inst-ss-rlb-01
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-14
    } else {
        // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-15
        // The upstream answered with something other than `101`: it produced a
        // response, so the upgrade is refused.
        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-15
        // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-16
        // The upstream refused the upgrade: its status, headers and body are
        // passed through unchanged, `X-OAGW-Error-Source: upstream` is stamped on
        // the head and no relay is started.
        let mut builder = Response::builder().status(raw.status());
        for (name, value) in raw.headers().iter() {
            builder = builder.header(name, value);
        }
        for (name, value) in
            crate::infra::proxy::validate::cors_response_headers(&cors, context.cross_origin)
        {
            builder = builder.header(name, value);
        }
        let refusal = match builder.body(Body::new(raw.into_body())) {
            Ok(response) => response,
            Err(_) => {
                return gateway_problem(
                    DomainError::ProtocolError {
                        detail: "the upstream refusal could not be relayed".to_owned(),
                    },
                    context,
                );
            }
        };
        let _ = record.advance_session(WsEvent::Refused);
        let _ = record.advance(StreamEvent::HeadRelayed);
        let _ = record.advance(StreamEvent::UpstreamEnded);
        StreamReply::Response(refusal, ERROR_SOURCE_UPSTREAM, Box::new(context))
        // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-16
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-15
        // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-15
    }
    // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-11
}
// @cpt-end:cpt-cf-oagw-dod-ws-upgrade:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-ws-relay:p1:inst-full
/// Which side of an established pair one relay direction reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelaySide {
    /// The client side: bytes read here travel to the upstream.
    Client,
    /// The upstream side: bytes read here travel to the client.
    Upstream,
}

/// The measurement one established pair shares
/// (`inst-ss-rlb-04`, `inst-ss-tmo-02`).
///
/// The byte counts land on the record entry 2.7 reads, and the activity stamp is
/// what the idle window is reset from.
struct RelayMeasure {
    record: SharedRecord,
    epoch: Instant,
    /// Milliseconds since `epoch` of the last byte in either direction.
    activity: AtomicU64,
    /// Milliseconds since `epoch` of the first empty read of the client side.
    client_eof: AtomicU64,
    /// Milliseconds since `epoch` of the first empty read of the upstream side.
    upstream_eof: AtomicU64,
}

impl RelayMeasure {
    fn new(record: SharedRecord) -> Self {
        Self {
            record,
            epoch: Instant::now(),
            activity: AtomicU64::new(0),
            client_eof: AtomicU64::new(0),
            upstream_eof: AtomicU64::new(0),
        }
    }

    /// Count the bytes read on `side` and reset the idle window
    /// (`inst-ss-rlb-04`, `inst-ss-tmo-02`).
    fn received(&self, side: RelaySide, bytes: u64) {
        match side {
            RelaySide::Client => self.record.count_upstream(bytes),
            RelaySide::Upstream => self.record.count_downstream(bytes),
        }
        // The first frame that crossed moves the session onto relaying
        // (`inst-ss-stw-04`): a session that is still established is the one
        // waiting for its first frame.
        if self.record.session_state() == Some(WsState::Established) {
            let _ = self.record.advance_session(WsEvent::FrameRelayed);
        }
        self.stamp();
    }

    /// Stamp the activity a byte forwarded in either direction produced.
    fn stamp(&self) {
        self.activity.store(self.elapsed_ms(), Ordering::Relaxed);
    }

    /// Record the first empty read of `side`.
    fn observed_eof(&self, side: RelaySide) {
        let target = match side {
            RelaySide::Client => &self.client_eof,
            RelaySide::Upstream => &self.upstream_eof,
        };
        let _ = target.compare_exchange(0, self.elapsed_ms(), Ordering::Relaxed, Ordering::Relaxed);
    }

    /// The milliseconds of the last byte, to compare across a window.
    fn activity_ms(&self) -> u64 {
        self.activity.load(Ordering::Relaxed)
    }

    /// The instant the next window is armed from: the last byte plus one window.
    fn activity_instant(&self, idle: Duration) -> tokio::time::Instant {
        let millis = self.activity_ms();
        tokio::time::Instant::from_std(self.epoch + Duration::from_millis(millis) + idle)
    }

    /// The side that closed the established pair first, from the first empty
    /// reads (`inst-ss-rlb-09`).
    fn closing_side(&self) -> &'static str {
        let client = self.client_eof.load(Ordering::Relaxed);
        let upstream = self.upstream_eof.load(Ordering::Relaxed);
        if upstream == 0 || (client != 0 && client <= upstream) {
            crate::domain::stream::SIDE_CLIENT
        } else {
            crate::domain::stream::SIDE_UPSTREAM
        }
    }

    /// The side that failed while the other was still open.
    fn failing_side(&self) -> &'static str {
        if self.upstream_eof.load(Ordering::Relaxed) != 0 {
            crate::domain::stream::SIDE_CLIENT
        } else {
            crate::domain::stream::SIDE_UPSTREAM
        }
    }

    /// The elapsed milliseconds, floored at one so a stamp is never `0`: `0` is
    /// the value the first-empty-read slots hold while a side is still open.
    fn elapsed_ms(&self) -> u64 {
        (self.epoch.elapsed().as_millis() as u64).max(1)
    }
}

/// The relay direction of one side of an established pair.
///
/// The direction counts the bytes it reads, stamps the activity every byte
/// produces and records the first empty read of its side, so the idle window and
/// the close reason are both decided from what actually crossed the connection.
struct Monitored<S> {
    inner: S,
    measure: Arc<RelayMeasure>,
    side: RelaySide,
}

impl<S> Monitored<S> {
    fn new(inner: S, measure: Arc<RelayMeasure>, side: RelaySide) -> Self {
        Self {
            inner,
            measure,
            side,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Monitored<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let read = buf.filled().len().saturating_sub(before);
            if read == 0 {
                this.measure.observed_eof(this.side);
            } else {
                this.measure.received(this.side, read as u64);
            }
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Monitored<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let written = ready!(Pin::new(&mut this.inner).poll_write(cx, buf))?;
        this.measure.stamp();
        Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// One side of an established pair, as the frame relay reads and writes it.
///
/// `hyper` speaks its own `rt` traits and the frame relay speaks `tokio`'s, so
/// the adapter bridges the two and holds the scratch buffer the read copies
/// through.
struct HyperIo {
    inner: hyper::upgrade::Upgraded,
    scratch: Box<[u8; SCRATCH_SIZE]>,
}

/// The read buffer of one relay direction, in bytes.
const SCRATCH_SIZE: usize = 8192;

impl HyperIo {
    fn new(inner: hyper::upgrade::Upgraded) -> Self {
        Self {
            inner,
            scratch: Box::new([0; SCRATCH_SIZE]),
        }
    }
}

impl AsyncRead for HyperIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let HyperIo { inner, scratch } = this;
        // The read window is never larger than the buffer the caller offered, so
        // a caller that hands over a small buffer cannot be overrun and no byte
        // is read that would have to be dropped.
        let capacity = buf.remaining().min(SCRATCH_SIZE);
        let mut window = hyper::rt::ReadBuf::new(&mut scratch[..capacity]);
        ready!(HyperRead::poll_read(Pin::new(inner), cx, window.unfilled()))?;
        buf.put_slice(window.filled());
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for HyperIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        HyperWrite::poll_write(Pin::new(&mut self.get_mut().inner), cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        HyperWrite::poll_flush(Pin::new(&mut self.get_mut().inner), cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        HyperWrite::poll_shutdown(Pin::new(&mut self.get_mut().inner), cx)
    }
}

/// How an established pair ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayEnd {
    /// Both directions ran to completion.
    Closed,
    /// No byte crossed either direction within the idle window.
    Idle,
    /// One direction failed while the other was still open.
    Lost,
}

/// Relay the frames of one established pair in both directions
/// (`cpt-cf-oagw-algo-ws-relay`).
///
/// Both directions are relayed by one bidirectional copy, the byte counts and the
/// close reason are recorded on the record entry 2.7 reads, and the idle window
/// is armed once and reset on every byte in either direction
/// (`inst-ss-rlb-04`, `inst-ss-rlb-08`, `inst-ss-tmo-02`).
///
/// # Errors
///
/// Never returns an error: every failure is recorded on the exchange record the
/// caller handed over, and the way the pair ended is returned for the tests.
async fn relay_frames(
    client: HyperIo,
    upstream: HyperIo,
    record: SharedRecord,
    mut close: ExchangeClose,
    idle: Duration,
) -> RelayEnd {
    let measure = Arc::new(RelayMeasure::new(record.clone()));
    let mut client = Monitored::new(client, measure.clone(), RelaySide::Client);
    let mut upstream = Monitored::new(upstream, measure.clone(), RelaySide::Upstream);
    let mut copy = Box::pin(tokio::io::copy_bidirectional(&mut client, &mut upstream));
    let mut seen = measure.activity_ms();
    let mut deadline = measure.activity_instant(idle);
    loop {
        match tokio::time::timeout_at(deadline, copy.as_mut()).await {
            Ok(result) => {
                let end = closed(result, &measure, &record);
                close.finish();
                return end;
            }
            Err(_) => {
                // A byte crossed since the window was armed: the window is
                // re-armed from the instant of that byte and the exchange keeps
                // running (`inst-ss-tmo-03`).
                let now = measure.activity_ms();
                if now != seen {
                    seen = now;
                    deadline = measure.activity_instant(idle);
                    continue;
                }
                // @cpt-begin:cpt-cf-oagw-algo-stream-timeout:p1:inst-ss-tmo-04
                // The window elapsed with no byte in either direction: the
                // exchange is torn down, the idle close reason is recorded with
                // the window that fired (`inst-ss-tmo-05`, `inst-ss-tmo-07`) and
                // both lifecycles land on their timed-out state
                // (`inst-ss-tmo-08`).
                let decision =
                    StreamErrorClassifier::classify(StreamFailure::IdleElapsed, true, true);
                record.record_error(&decision.error);
                let _ = record.advance(StreamEvent::IdleElapsed);
                let _ = record.advance_session(WsEvent::IdleElapsed);
                close.finish();
                return RelayEnd::Idle;
                // @cpt-end:cpt-cf-oagw-algo-stream-timeout:p1:inst-ss-tmo-04
            }
        }
    }
}

/// Record the terminal outcome of a relay that finished
/// (`cpt-cf-oagw-flow-stream-close`, `cpt-cf-oagw-flow-stream-error`).
fn closed(
    result: io::Result<(u64, u64)>,
    measure: &RelayMeasure,
    record: &SharedRecord,
) -> RelayEnd {
    match result {
        // @cpt-begin:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-05
        // @cpt-begin:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-06
        // The pair ran to completion: whoever closed it first is recorded as the
        // closing side, the close reason follows that side, and both lifecycles
        // land on their terminal state.
        Ok(_) => {
            // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-13
            // The side that closed first decides the close: a client that went
            // away has the far side closed with it and the close is recorded as
            // client-initiated, an upstream that finished ends the exchange.
            let side = measure.closing_side();
            if side == crate::domain::stream::SIDE_CLIENT {
                let _ = record.advance(StreamEvent::Failed {
                    side,
                    reason: CloseReason::ClientDisconnected,
                });
            } else {
                let _ = record.advance(StreamEvent::UpstreamEnded);
            }
            let _ = record.advance_session(WsEvent::Closed { side });
            // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-13
            RelayEnd::Closed
            // @cpt-end:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-06
            // @cpt-end:cpt-cf-oagw-flow-stream-close:p1:inst-ss-ucl-05
        }
        // @cpt-begin:cpt-cf-oagw-flow-stream-error:p1:inst-ss-err-08
        // One direction failed while the other was still open: the exchange is
        // torn down and the failing direction is recorded with it.
        Err(_) => {
            let side = measure.failing_side();
            let _ = record.advance(StreamEvent::Failed {
                side,
                reason: CloseReason::Aborted,
            });
            let _ = record.advance_session(WsEvent::Failed {
                side,
                reason: CloseReason::Aborted,
            });
            RelayEnd::Lost
            // @cpt-end:cpt-cf-oagw-flow-stream-error:p1:inst-ss-err-08
        }
    }
}

/// Relay one established pair from the two upgrade handles.
///
/// Both sides are awaited — the client side is the handle the request extensions
/// carried, the upstream side the one the `101` carries — and the relay runs
/// until the pair is closed, the client goes away or the idle window elapses.
async fn relay_session(
    client: Option<hyper::upgrade::OnUpgrade>,
    upstream: hyper::upgrade::OnUpgrade,
    record: SharedRecord,
    mut close: ExchangeClose,
    idle: Duration,
) -> RelayEnd {
    let Some(client) = client else {
        // @cpt-begin:cpt-cf-oagw-flow-stream-disconnect:p1:inst-ss-cdc-02
        // No client handle: the caller never asked for the protocol the gateway
        // negotiated, so there is nothing to relay and the exchange is aborted.
        let _ = record.advance(StreamEvent::Failed {
            side: crate::domain::stream::SIDE_CLIENT,
            reason: CloseReason::Aborted,
        });
        let _ = record.advance_session(WsEvent::Failed {
            side: crate::domain::stream::SIDE_CLIENT,
            reason: CloseReason::Aborted,
        });
        close.finish();
        return RelayEnd::Lost;
        // @cpt-end:cpt-cf-oagw-flow-stream-disconnect:p1:inst-ss-cdc-02
    };
    // @cpt-begin:cpt-cf-oagw-algo-ws-relay:p1:inst-ss-rlb-02
    // Both sides are awaited, bounded by the same idle window the relay runs
    // under: a side that never upgrades cannot hold the negotiated exchange open
    // for longer than a silent exchange is allowed to last, and a side that
    // never upgrades aborts the relay before any frame is moved.
    let negotiation =
        tokio::time::timeout(idle, futures_util::future::try_join(client, upstream));
    let (client, upstream) = match negotiation.await {
        Ok(Ok((client, upstream))) => (HyperIo::new(client), HyperIo::new(upstream)),
        _ => {
            let _ = record.advance(StreamEvent::Failed {
                side: crate::domain::stream::SIDE_UPSTREAM,
                reason: CloseReason::Aborted,
            });
            let _ = record.advance_session(WsEvent::Failed {
                side: crate::domain::stream::SIDE_UPSTREAM,
                reason: CloseReason::Aborted,
            });
            close.finish();
            return RelayEnd::Lost;
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-ws-relay:p1:inst-ss-rlb-02

    // @cpt-begin:cpt-cf-oagw-algo-ws-relay:p1:inst-ss-rlb-03
    // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-09
    relay_frames(client, upstream, record, close, idle).await
    // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-09
    // @cpt-end:cpt-cf-oagw-algo-ws-relay:p1:inst-ss-rlb-03
}
// @cpt-end:cpt-cf-oagw-dod-ws-relay:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;

    use http::header::HeaderName;
    use tokio::time::Duration as TokioDuration;

    /// A body that never yields a frame: the silence the idle window measures.
    fn silent_body() -> toolkit_http::ResponseBody {
        use http_body_util::BodyExt as _;
        http_body_util::StreamBody::new(futures_util::stream::pending::<Result<_, BodyError>>())
            .boxed()
    }

    /// A body that yields `count` chunks, one every `period`, and then ends: the
    /// drip of an upstream that keeps a stream alive below the idle window.
    fn drip_body(count: usize, period: TokioDuration) -> toolkit_http::ResponseBody {
        let stream = futures_util::stream::unfold(0usize, move |sent| async move {
            if sent >= count {
                return None;
            }
            tokio::time::sleep(period).await;
            // `hyper` re-exports the body frame type, so the test names no crate
            // the gear does not already depend on.
            let frame = hyper::body::Frame::data(Bytes::from_static(b"data: x\n\n"));
            Some((Ok::<_, BodyError>(frame), sent + 1))
        });
        http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream))
    }

    fn headers<const N: usize>(pairs: [(&'static str, &'static str); N]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        headers
    }

    fn upgrade_request() -> HeaderMap {
        headers([
            ("upgrade", "websocket"),
            ("connection", "Upgrade"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
        ])
    }

    #[test]
    fn a_stream_is_detected_from_its_media_type() {
        let facts = SseClassifier::classify(None, Some("text/event-stream"));
        assert!(facts.is_stream());
        assert_eq!(facts.kind(), Some(StreamKind::SseResponse));
    }

    #[test]
    fn a_streamed_media_type_is_detected_ignoring_parameters_and_case() {
        let facts = SseClassifier::classify(None, Some("Text/Event-Stream; charset=utf-8"));
        assert!(facts.is_stream());
        assert!(!facts.request);
    }

    #[test]
    fn a_request_that_asks_for_a_stream_is_a_streamed_request_fact() {
        let facts = SseClassifier::classify(Some("text/event-stream"), None);
        assert!(!facts.is_stream());
        assert_eq!(facts.kind(), Some(StreamKind::SseRequest));
    }

    #[test]
    fn an_sse_request_with_a_non_sse_response_stays_on_the_buffered_path() {
        let facts = SseClassifier::classify(Some("text/event-stream"), Some("application/json"));
        assert!(!facts.is_stream());
        assert!(facts.request);
    }

    #[test]
    fn a_streamed_response_without_a_streamed_request_is_still_streamed() {
        let facts = SseClassifier::classify(Some("application/json"), Some("text/event-stream"));
        assert!(facts.is_stream());
    }

    #[test]
    fn a_missing_content_type_is_not_a_streamed_response() {
        assert!(!SseClassifier::classify(None, None).is_stream());
        assert_eq!(SseClassifier::classify(None, None).kind(), None);
    }

    #[test]
    fn a_media_type_that_only_shares_a_prefix_is_not_streamed() {
        assert!(!SseClassifier::classify(None, Some("text/event-stream+xml")).is_stream());
        assert!(!SseClassifier::classify(None, Some("text/event")).is_stream());
    }

    #[test]
    fn a_well_formed_upgrade_request_is_admitted() {
        assert!(validate_upgrade_headers(&upgrade_request()).is_ok());
    }

    #[test]
    fn an_upgrade_without_a_websocket_upgrade_header_is_refused() {
        let mut request = upgrade_request();
        request.insert(UPGRADE, HeaderValue::from_static("h2c"));
        assert!(validate_upgrade_headers(&request).is_err());
    }

    #[test]
    fn an_upgrade_without_a_connection_upgrade_header_is_refused() {
        let mut request = upgrade_request();
        request.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        assert!(validate_upgrade_headers(&request).is_err());
    }

    #[test]
    fn an_upgrade_without_a_key_is_refused() {
        let mut request = upgrade_request();
        request.remove(WS_KEY);
        assert!(validate_upgrade_headers(&request).is_err());
    }

    #[test]
    fn an_upgrade_with_an_unsupported_version_is_refused() {
        let mut request = upgrade_request();
        request.insert(WS_VERSION, HeaderValue::from_static("8"));
        assert!(validate_upgrade_headers(&request).is_err());
    }

    #[test]
    fn an_upgrade_refusal_names_the_row_the_entry_2_1_layer_maps() {
        let error = validate_upgrade_headers(&HeaderMap::new()).unwrap_err();
        assert!(matches!(error, DomainError::ValidationError { .. }));
    }

    #[test]
    fn the_reinjected_header_set_carries_exactly_the_declared_upgrade_headers() {
        let mut outbound = headers([("x-oagw-forwarded-for", "203.0.113.7")]);
        let client = headers([
            ("upgrade", "websocket"),
            ("connection", "keep-alive"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("sec-websocket-protocol", "chat"),
            ("sec-websocket-extensions", "permessage-deflate"),
        ]);
        inject_upgrade_headers(&mut outbound, &client);
        let names: Vec<&str> = outbound.keys().map(|name| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "x-oagw-forwarded-for",
                "upgrade",
                "connection",
                "sec-websocket-key",
                "sec-websocket-version",
                "sec-websocket-protocol",
                "sec-websocket-extensions",
            ]
        );
    }

    #[test]
    fn the_reinjection_restores_the_hop_by_hop_pair_the_transform_stripped() {
        let mut outbound = HeaderMap::new();
        inject_upgrade_headers(&mut outbound, &upgrade_request());
        assert_eq!(outbound.get(UPGRADE).and_then(|v| v.to_str().ok()), Some("websocket"));
        assert_eq!(outbound.get(CONNECTION).and_then(|v| v.to_str().ok()), Some("Upgrade"));
    }

    #[test]
    fn the_reinjection_does_not_invent_a_negotiated_header_the_client_omitted() {
        let mut outbound = HeaderMap::new();
        inject_upgrade_headers(&mut outbound, &upgrade_request());
        assert!(outbound.get(WS_PROTOCOL).is_none());
        assert!(outbound.get(WS_EXTENSIONS).is_none());
    }

    #[test]
    fn no_sec_websocket_accept_is_injected_by_the_gateway() {
        let mut outbound = HeaderMap::new();
        inject_upgrade_headers(&mut outbound, &upgrade_request());
        assert!(outbound.get(WS_ACCEPT).is_none());
        outbound.insert(WS_ACCEPT, HeaderValue::from_static("stale"));
        inject_upgrade_headers(&mut outbound, &upgrade_request());
        assert!(outbound.get(WS_ACCEPT).is_none());
    }

    #[test]
    fn the_record_of_a_streamed_exchange_carries_the_endpoint_and_the_window() {
        let endpoint = crate::domain::model::Endpoint {
            scheme: crate::domain::model::Scheme::Https,
            host: "stub.internal".to_owned(),
            port: 443,
        };
        let record = open_record(StreamKind::SseResponse, Some(&endpoint), TokioDuration::from_secs(7));
        assert_eq!(record.kind(), StreamKind::SseResponse);
        assert_eq!(record.endpoint(), Some("stub.internal:443"));
        assert_eq!(record.idle_window_secs(), Some(7));
        assert_eq!(record.session_state(), None);
    }

    #[test]
    fn the_record_of_an_upgrade_carries_a_websocket_session() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_secs(7));
        assert_eq!(record.kind(), StreamKind::WebSocket);
        assert!(record.session_state().is_some());
        assert_eq!(record.endpoint(), None);
    }

    #[tokio::test]
    async fn chunks_that_keep_arriving_inside_the_window_hold_the_relay_open() {
        // Six chunks, one every 10 ms, against a 40 ms window: the drip runs
        // longer than the window, so only a window that is reset on every
        // arriving chunk keeps the exchange open (`inst-ss-tmo-03`). A window
        // that is never reset would tear the exchange down after the first
        // silence, and the loop below would stop at the first chunk.
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_millis(40));
        let mut relay = SseRelay::new(
            drip_body(6, TokioDuration::from_millis(10)),
            record.clone(),
            TokioDuration::from_millis(40),
        );
        let mut relayed = 0usize;
        // The head is committed before the first chunk is flushed, as the
        // transport does when it streams the handoff out.
        relay.head_committed = true;
        let _ = record.advance(StreamEvent::HeadRelayed);
        loop {
            match std::future::poll_fn(|cx| relay.poll_chunk(cx)).await {
                Some(Ok(bytes)) => {
                    assert_eq!(bytes.as_ref(), b"data: x\n\n");
                    relayed += 1;
                }
                Some(Err(_)) => panic!("a drip inside the window is never read as an abort"),
                None => break,
            }
        }
        assert_eq!(relayed, 6, "every chunk arrived inside the window");
        assert_eq!(record.state(), crate::domain::stream::StreamState::Closed);
        let outcome = record.outcome().expect("the ended exchange records it");
        assert_eq!(outcome.reason, CloseReason::UpstreamClosed);
        assert_eq!(outcome.bytes_downstream, 54, "every byte was counted");
        assert_eq!(
            relay.relayed,
            54,
            "the relay counted the bytes it handed downstream"
        );
    }

    /// The measurement the re-arm loop of the established relay keys on: a byte
    /// stamps the activity, the count lands on the side it was read from and the
    /// next window is armed from the stamped instant.
    #[tokio::test]
    async fn a_forwarded_byte_stamps_the_activity_the_window_rearms_from() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_millis(30));
        // The relay runs on an established session: the `101` head was relayed
        // before the first frame crossed.
        let _ = record.advance_session(WsEvent::Established);
        let measure = RelayMeasure::new(record.clone());
        assert_eq!(measure.activity_ms(), 0, "no byte crossed yet");
        tokio::time::sleep(TokioDuration::from_millis(2)).await;
        measure.received(RelaySide::Upstream, 7);
        let first = measure.activity_ms();
        assert!(first >= 1, "the stamp is floored at one, so it is never 0");
        tokio::time::sleep(TokioDuration::from_millis(2)).await;
        measure.received(RelaySide::Client, 3);
        let second = measure.activity_ms();
        assert!(second > first, "a later byte stamps a later instant");
        // The count a side produced lands on that side: a read from the upstream
        // is a byte the client will receive and a read from the client one the
        // upstream will receive.
        let _ = record.advance(StreamEvent::HeadRelayed);
        let _ = record.advance(StreamEvent::ChunkFlushed);
        let _ = record.advance(StreamEvent::UpstreamEnded);
        let outcome = record.outcome().expect("the ended exchange records it");
        assert_eq!(outcome.bytes_downstream, 7);
        assert_eq!(outcome.bytes_upstream, 3);
        // The frame that crossed moved the session onto relaying: the state
        // entry 2.7 reports is the one the byte traffic produced.
        assert_eq!(
            record.session_state(),
            Some(crate::domain::stream::WsState::Relaying)
        );
        // The next window is armed from the stamped byte: it starts one window
        // after the instant the byte crossed, never from an earlier moment.
        let idle = TokioDuration::from_millis(30);
        let armed = measure.activity_instant(idle);
        let from_last_byte = tokio::time::Instant::from_std(
            measure.epoch + std::time::Duration::from_millis(second) + idle,
        );
        assert_eq!(armed, from_last_byte);
    }

    #[tokio::test]
    async fn an_idle_window_that_elapses_records_the_timed_out_state() {
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_secs(30));
        let mut relay = SseRelay::new(silent_body(), record.clone(), TokioDuration::from_millis(5));
        let chunk = std::future::poll_fn(|cx| relay.poll_chunk(cx)).await;
        assert!(chunk.is_none(), "the elapsed window ends the body");
        assert_eq!(record.state(), crate::domain::stream::StreamState::TimedOut);
        let outcome = record.outcome().expect("the timed-out exchange records it");
        assert_eq!(outcome.reason, CloseReason::IdleTimeout);
        assert_eq!(outcome.closing_side, crate::domain::stream::SIDE_NONE);
        assert!(outcome.error_type.is_some());
    }

    /// A body that yields the given chunks and then loses the exchange: the
    /// mid-stream abort an upstream reset produces after the head was committed.
    fn body_then_error(chunks: &'static [&'static [u8]]) -> toolkit_http::ResponseBody {
        use futures_util::StreamExt as _;
        let frames = chunks
            .iter()
            // `hyper` re-exports the body frame type, so the test names no
            // crate the gear does not already depend on.
            .map(|chunk| Ok::<_, BodyError>(hyper::body::Frame::data(Bytes::from_static(chunk))))
            .collect::<Vec<_>>();
        let stream = futures_util::stream::iter(frames).chain(futures_util::stream::once(
            async {
                Err::<hyper::body::Frame<Bytes>, BodyError>(
                    std::io::Error::other("the upstream lost the exchange").into(),
                )
            },
        ));
        http_body_util::BodyExt::boxed(http_body_util::StreamBody::new(stream))
    }

    #[tokio::test]
    async fn an_upstream_body_error_before_the_first_byte_maps_the_aborted_row() {
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_secs(30));
        let mut relay = SseRelay::new(silent_body(), record.clone(), TokioDuration::from_secs(30));
        relay.classify(StreamFailure::UpstreamLoss);
        let decision = StreamErrorClassifier::classify(StreamFailure::UpstreamLoss, false, false);
        assert!(decision.problem_writable);
        assert_eq!(decision.closing_side, crate::domain::stream::SIDE_UPSTREAM);
        relay.tear_down(decision, true);
        assert_eq!(record.state(), crate::domain::stream::StreamState::Aborted);
        let outcome = record.outcome().expect("the aborted exchange records it");
        assert_eq!(outcome.reason, CloseReason::Aborted);
    }

    #[tokio::test]
    async fn an_abort_after_the_head_was_committed_writes_no_problem_document() {
        // The exchange had committed its head and relayed one chunk when the
        // upstream lost the body: the relay classifies the loss itself, so the
        // decision must carry no writable problem document and the record must
        // name the abort with the error type entry 2.7 reports.
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_secs(30));
        let mut relay = SseRelay::new(
            body_then_error(&[b"data: one\n\n", b"data: two\n\n"]),
            record.clone(),
            TokioDuration::from_secs(30),
        );
        let first = std::future::poll_fn(|cx| relay.poll_chunk(cx)).await;
        assert_eq!(
            first
                .expect("the first chunk")
                .expect("the chunk is a body byte")
                .as_ref(),
            b"data: one\n\n"
        );
        relay.head_committed = true;
        assert!(relay.body_flushed(), "the first chunk was flushed");

        let second = std::future::poll_fn(|cx| relay.poll_chunk(cx)).await;
        assert_eq!(
            second
                .expect("the second chunk")
                .expect("the chunk is a body byte")
                .as_ref(),
            b"data: two\n\n"
        );
        let third = std::future::poll_fn(|cx| relay.poll_chunk(cx)).await;
        assert!(
            matches!(third, Some(Err(_))),
            "the lost body ends the relay with the error it read"
        );

        let decision = relay.stop.as_ref().expect("the relay recorded the stop");
        assert!(
            !decision.problem_writable,
            "the head is committed, so no problem document may be written"
        );
        assert!(matches!(decision.error, DomainError::StreamAborted { .. }));
        assert_eq!(record.state(), crate::domain::stream::StreamState::Aborted);
        let outcome = record.outcome().expect("the aborted exchange records it");
        assert_eq!(outcome.reason, CloseReason::Aborted);
        assert_eq!(outcome.closing_side, crate::domain::stream::SIDE_UPSTREAM);
        assert!(
            outcome.error_type.is_some(),
            "the error type is recorded for entry 2.7"
        );
        assert_eq!(outcome.bytes_downstream, 22, "the two chunks were counted");
        assert_eq!(relay.relayed, 22, "the relay counted the bytes it handed out");
    }

    #[tokio::test]
    async fn a_relay_that_is_dropped_records_the_client_disconnection() {
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_secs(30));
        let relay = SseRelay::new(silent_body(), record.clone(), TokioDuration::from_secs(30));
        drop(relay);
        let outcome = record.outcome().expect("the dropped relay records it");
        assert_eq!(outcome.reason, CloseReason::ClientDisconnected);
        assert_eq!(outcome.closing_side, crate::domain::stream::SIDE_CLIENT);
        assert_eq!(record.state(), crate::domain::stream::StreamState::Aborted);
    }

    #[tokio::test]
    async fn a_relay_that_already_ended_records_nothing_more_when_dropped() {
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_secs(30));
        let mut relay = SseRelay::new(silent_body(), record.clone(), TokioDuration::from_secs(30));
        let _ = record.advance(StreamEvent::HeadRelayed);
        relay.ended();
        drop(relay);
        let outcome = record.outcome().expect("the clean close is kept");
        assert_eq!(outcome.reason, CloseReason::UpstreamClosed);
        assert_eq!(outcome.closing_side, crate::domain::stream::SIDE_UPSTREAM);
    }

    #[tokio::test]
    async fn a_relay_counts_the_bytes_it_forwards_downstream() {
        let record = open_record(StreamKind::SseResponse, None, TokioDuration::from_secs(30));
        let mut relay = SseRelay::new(silent_body(), record.clone(), TokioDuration::from_secs(30));
        let chunk = Bytes::from_static(b"data: hello\n\n");
        relay.relayed += chunk.len() as u64;
        record.count_downstream(chunk.len() as u64);
        assert!(relay.body_flushed());
        assert_eq!(relay.relayed, 13);
        let _ = record.advance(StreamEvent::HeadRelayed);
        let _ = record.advance(StreamEvent::UpstreamEnded);
        let outcome = record.outcome().expect("the byte count is recorded");
        assert_eq!(outcome.bytes_downstream, 13);
        assert_eq!(outcome.bytes_upstream, 0);
    }

    #[test]
    fn the_measure_reads_the_side_that_emptied_first_as_the_closing_side() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_secs(30));
        let measure = RelayMeasure::new(record);
        measure.observed_eof(RelaySide::Upstream);
        measure.observed_eof(RelaySide::Client);
        measure.received(RelaySide::Client, 4);
        assert_eq!(measure.closing_side(), crate::domain::stream::SIDE_CLIENT);
    }

    #[test]
    fn the_measure_reads_the_upstream_side_as_closing_when_only_it_emptied() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_secs(30));
        let measure = RelayMeasure::new(record);
        measure.observed_eof(RelaySide::Upstream);
        assert_eq!(measure.closing_side(), crate::domain::stream::SIDE_UPSTREAM);
    }

    #[test]
    fn the_measure_counts_the_bytes_each_direction_relayed() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_secs(30));
        let measure = RelayMeasure::new(record.clone());
        measure.received(RelaySide::Client, 11);
        measure.received(RelaySide::Upstream, 7);
        let _ = record.advance(StreamEvent::Failed {
            side: crate::domain::stream::SIDE_CLIENT,
            reason: CloseReason::ClientDisconnected,
        });
        let outcome = record.outcome().expect("the relay records it");
        assert_eq!(outcome.bytes_upstream, 11);
        assert_eq!(outcome.bytes_downstream, 7);
    }

    #[test]
    fn the_measure_fails_the_upstream_side_while_the_client_is_still_open() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_secs(30));
        let measure = RelayMeasure::new(record);
        assert_eq!(measure.failing_side(), crate::domain::stream::SIDE_UPSTREAM);
    }

    #[test]
    fn the_measure_arms_the_next_window_from_the_last_byte() {
        let record = open_record(StreamKind::WebSocket, None, TokioDuration::from_secs(30));
        let measure = RelayMeasure::new(record);
        let idle = TokioDuration::from_millis(50);
        let armed = measure.activity_instant(idle);
        assert!(armed <= tokio::time::Instant::now() + idle);
        assert!(armed > tokio::time::Instant::now());
        assert_eq!(measure.activity_ms(), 0, "a silent relay has no byte yet");
    }
}
