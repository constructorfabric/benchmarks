//! The data-plane streaming stage of `cpt-cf-oagw-feature-streaming-proxy`.
//!
//! The module is the stage of [`crate::infra::proxy::pipeline`] that the proxy
//! path enters after the outbound call came back: it classifies the response a
//! stream and hands the streamed body its idle-read clock and its failure
//! recorder, and it relays the 101 of a verified RFC 6455 handshake and pumps
//! the upgraded session bidirectionally. The pipeline is the caller of both,
//! the API handler being the caller of the pump through the pipeline.
//!
//! Everything here is byte-level: no frame is buffered, no frame is re-encoded
//! and no frame's content is interpreted. The decisions the stage acts on are
//! the pure ones of [`crate::domain::streaming`]; the strip list it benefits
//! from is the base pipeline's, the rows it records are the closed table's and
//! the one timeout it is described by is the `proxy_timeout_secs` the pipeline
//! carries in its limits.
// @cpt-begin:cpt-cf-oagw-dod-sse-passthrough:p1:inst-full
// The SSE passthrough contract of `cpt-cf-oagw-dod-sse-passthrough`: the
// stream is detected on the upstream response only — a `Content-Type` of
// `text/event-stream`, or any non-buffered stream the bridge hands over — and
// never on the client request; the body frames are forwarded incrementally as
// the upstream produces them with the upstream `Content-Type` passed through
// unchanged and no frame buffered; the lifecycle the requirement names is
// open, close and error; no retry of any kind is issued, the client request is
// never re-issued and no streamed body is ever buffered; an aborted stream is
// classified 502 `StreamAborted` and an idle read on a live stream 504
// `IdleTimeout` through the existing rows; and the relayed streamed head
// carries `X-OAGW-Error-Source: upstream`, the header the API handler adds.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, StatusCode};
use hyper::body::Body as _;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::domain::streaming::{StreamFailure, StreamSession, StreamSessionState};
use crate::infra::proxy::pipeline::{OutboundBody, OutboundReply};

/// The recorder a streamed body reports a classified failure to, the closure
/// the streaming stage attaches so the failure reaches the journal while the
/// body is being pumped by hyper.
pub type FailureRecorder = Arc<dyn Fn(StreamFailure) + Send + Sync>;

/// The `Content-Type` value that marks an upstream response an SSE stream.
const SSE_CONTENT_TYPE: &str = "text/event-stream";

/// The capacity of the streamed-failure journal, the oldest event being
/// dropped first: the journal is the classification record the instruments of
/// the observability feature read, not a log of any request's content.
const JOURNAL_CAPACITY: usize = 256;

/// The byte size of one relay read, the chunk a pump direction writes as it
/// arrives.
const RELAY_CHUNK: usize = 8 * 1024;

/// One classified streamed failure, carrying identifiers and the mapped row
/// and never a header value, a body, a query string, a credential or a frame.
#[derive(Debug, Clone)]
pub struct StreamEvent {
    /// The normalized alias the request resolved to.
    pub alias: String,
    /// The configuration host of the endpoint the stream was established to.
    pub endpoint_host: String,
    /// The closed-table row the failure was mapped onto.
    pub row: String,
    /// The state the stream session was in when the failure was classified.
    pub state: StreamSessionState,
}

/// The in-memory journal of the streamed failures this feature classifies.
///
/// It is the hook the emission of `cpt-cf-oagw-feature-observability` reads:
/// this feature records the classification and emits no instrument, no audit
/// record and no log line itself. The journal holds no state between requests
/// beyond the last `JOURNAL_CAPACITY` events and is never persisted.
#[derive(Debug, Default)]
pub struct StreamJournal {
    events: Mutex<VecDeque<StreamEvent>>,
}

impl StreamJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one classification, dropping the oldest event past the
    /// capacity.
    pub(crate) fn record(&self, event: StreamEvent) {
        let mut events = self.events.lock();
        if events.len() >= JOURNAL_CAPACITY {
            events.pop_front();
        }
        events.push_back(event);
    }

    /// The classifications recorded so far, oldest first.
    #[must_use]
    pub fn snapshot(&self) -> Vec<StreamEvent> {
        self.events.lock().iter().cloned().collect()
    }

    /// How many classifications the journal holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }

    /// Whether the journal holds no classification.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }
}

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-01
// Receive, from `cpt-cf-oagw-flow-proxy-request` at its streaming stage — the
// caller of this and every later step — the established outbound call's
// response head and body stream, together with the `ProxyContext` the pipeline
// opened at `inst-pf-01`, the selected `Endpoint` and the arrival instant the
// establishment timing is measured from; every stage of the pipeline before
// this one has already run and none is re-run here. `handover` is that
// receipt: it takes the reply the dispatch produced, the session the pipeline
// opened for the request and the alias and endpoint host the classification
// records name.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-01

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-02
// IF the upstream response head carries `Content-Type: text/event-stream`, or
// the bridge hands over a response whose body is a non-buffered stream rather
// than a body the passthrough can write at once — the stream being detected on
// the upstream response only, never on the client request, an `Accept:
// text/event-stream` request header alone never making a response a stream.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-02

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-03
// Classify the response as a stream and continue in this flow, the detection
// being this feature's and the classification of the response never altering
// the upstream's status, its headers or its `Content-Type` value.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-03

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-04
// ELSE report "not a stream" to the caller and RETURN: the buffered
// passthrough of `cpt-cf-oagw-flow-proxy-request` at `inst-pf-37` handles the
// response as received and this flow is done.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-04

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-05
// Relay the upstream response head to the caller as received — the upstream
// `Content-Type` passed through unchanged and never rewritten, the
// `response.*` rules of the selected upstream's `upstream.headers` block
// applied by the caller per `inst-pf-37`, and `X-OAGW-Error-Source: upstream`
// added because the body originates at `cpt-cf-oagw-actor-upstream-service` —
// with no retry, no buffer and no re-issuance of the client request anywhere
// in the path, per `cpt-cf-oagw-principle-no-retry`.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-05

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-06
// FOR EACH body frame the bridge delivers, write it to the client connection
// as it arrives, without accumulating frames, without waiting for the stream
// to end, and without re-framing, re-chunking or re-encoding an SSE event. The
// body is handed over below as the streamed body hyper writes through the
// passthrough, so the frame loop is the one the passthrough runs and no
// accumulation, no wait for the end and no re-framing is performed by this
// stage.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-06

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-07
// Hold the idle-read clock over the established stream: FOR EACH interval
// between received bytes, compare it against `proxy_timeout_secs` — the same
// value the establishment is bounded by, applied here as the maximum interval
// without a received byte on an established stream per §1.5. The clock is the
// one the streamed body already holds, re-timed to the configured value.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-07

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-08
// IF an idle-read interval reached `proxy_timeout_secs` without a received
// byte — the branch an established stream enters when the clock the streamed
// body holds expires.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-08

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-09
// Classify the failure through `cpt-cf-oagw-algo-stream-failure-classification`
// — 504 `IdleTimeout` — close the upstream connection and the client
// connection, and record the classification for the instruments
// `cpt-cf-oagw-feature-observability` emits, no problem+json body being
// written onto a connection whose streamed head was already forwarded.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-09

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-10
// Close the `ProxyContext` in `Failed` and RETURN the classification to
// `cpt-cf-oagw-flow-proxy-request`: the context is closed with the request
// that produced it and the session of §4 records the failure here.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-10

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-11
// ELSE IF the upstream connection failed or was closed before the upstream
// ended the body, the classification being the abort branch of the same
// algorithm.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-11

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-12
// Classify the failure through the same algorithm — 502 `StreamAborted` —
// close both connections, and never re-issue the request, never retry the
// stream and never re-connect by the gateway's own decision, per
// `cpt-cf-oagw-principle-no-retry`; a failure classified before the streamed
// head was forwarded is rendered as `application/problem+json` by
// `cpt-cf-oagw-flow-error-response` of `cpt-cf-oagw-feature-gear-wiring`
// instead.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-12

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-13
// Close the `ProxyContext` in `Failed` and RETURN the classification to
// `cpt-cf-oagw-flow-proxy-request`, the close of the abort branch being the
// same close the idle branch performs: the context is discarded with the
// request, the classification having been recorded.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-13

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-14
// ELSE the upstream ended the body: complete the stream, flush any final bytes
// the bridge already delivered, and close the client connection in the
// direction the upstream closed, which is the close half of the lifecycle the
// requirement names.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-14

// @cpt-begin:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-15
// Close the `ProxyContext` in `Responded` and RETURN the streamed outcome to
// `cpt-cf-oagw-flow-proxy-request`, the streamed response being counted by
// `oagw_requests_total` and `oagw_request_duration_seconds` like any other
// passthrough, its emission belonging to
// `cpt-cf-oagw-feature-observability`.
// @cpt-end:cpt-cf-oagw-flow-sse-passthrough:p1:inst-sse-15

/// Reports whether the upstream response is a stream the passthrough forwards
/// frame by frame rather than a body it writes at once.
///
/// The detection is on the response only: an `Accept: text/event-stream`
/// request header alone never makes a response a stream.
#[must_use]
pub fn is_streamed(reply: &OutboundReply) -> bool {
    let streamed_content_type = reply.headers.get("content-type").and_then(|value| {
        value.to_str().ok().map(|value| {
            value
                .split(';')
                .any(|token| token.trim().eq_ignore_ascii_case(SSE_CONTENT_TYPE))
        })
    });
    streamed_content_type.unwrap_or(false) || reply.body.size_hint().exact().is_none()
}

/// The disposition the streaming stage reports back to the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handover {
    /// The reply is not a stream: the buffered passthrough handles it
    /// (`inst-sse-04`).
    Buffered,
    /// The reply is a stream: the head is relayed and the body forwards the
    /// frames as the upstream produces them (`inst-sse-05` and `inst-sse-06`).
    Streamed,
}

/// Receives the established outbound call's response (`inst-sse-01`) and hands
/// the streamed body its idle-read clock and its failure recorder.
///
/// The head is relayed exactly as the upstream produced it: this function
/// alters no status, no header and no `Content-Type` value, it only decides
/// which half of the passthrough handles the body and arms the failure
/// recording the idle-read and abort branches of the flow report through.
pub fn handover(
    reply: &mut OutboundReply,
    session: &StreamSession,
    journal: &Arc<StreamJournal>,
    alias: &str,
    endpoint_host: &str,
    idle: Duration,
) -> Handover {
    if !is_streamed(reply) {
        // `inst-sse-04`: not a stream, the buffered passthrough handles it.
        let _ = session.transition(StreamSessionState::Completed);
        return Handover::Buffered;
    }
    // `inst-sse-03`: the response is classified a stream and the flow continues.
    let _ = session.transition(StreamSessionState::Detected);
    // An SSE stream reaches `Establishing` only as it reaches the head relay,
    // which is the whole establishment an SSE stream performs (`inst-ss-03`).
    let _ = session.transition(StreamSessionState::Establishing);
    // `inst-sse-05`: the head is relayed as received, the stream is open.
    let _ = session.transition(StreamSessionState::Streaming);
    if let OutboundBody::Streaming(body) = &mut reply.body {
        // `inst-sse-06` and `inst-sse-07`: the frames keep flowing through the
        // body hyper pumps, under the idle-read clock the configured value
        // bounds.
        body.set_idle(idle);
        let journal = Arc::clone(journal);
        let recorder_session = session.clone();
        let alias = alias.to_owned();
        let endpoint_host = endpoint_host.to_owned();
        body.set_recorder(Arc::new(move |failure| {
            record_failure(&journal, &recorder_session, &alias, &endpoint_host, failure);
        }));
    }
    Handover::Streamed
}

/// Classifies a failure an established stream or session produced and records
/// the classification (`inst-sse-08` to `inst-sse-13`, and `inst-ws-16` to
/// `inst-ws-18`).
///
/// Nothing of the streamed response or of the upgrade can be delivered any
/// more once the head or the 101 was relayed, so the row is recorded for the
/// instruments of the observability feature and both directions are closed:
/// no problem+json body is written onto a connection whose head or 101 was
/// already forwarded.
fn record_failure(
    journal: &StreamJournal,
    session: &StreamSession,
    alias: &str,
    endpoint_host: &str,
    failure: StreamFailure,
) {
    // `inst-sse-08`: the idle branch of an established stream, and the same
    // interval the pump measures on an established session.
    // `inst-sse-11`: the abort branch, the upstream connection having failed
    // or been closed before the upstream ended the body.
    let outcome = crate::domain::streaming::classify(failure, true);
    // `inst-sse-09` and `inst-sse-12`: the classification, 504 `IdleTimeout`
    // for an idle read and 502 `StreamAborted` for an abort, is recorded —
    // never re-issued, never retried, never re-connected by the gateway.
    journal.record(StreamEvent {
        alias: alias.to_owned(),
        endpoint_host: endpoint_host.to_owned(),
        row: outcome.row.mapping().variant.to_owned(),
        state: session.state(),
    });
    // `inst-sse-10` and `inst-sse-13`, and `inst-ws-18`: the context is closed
    // in `Failed` and the classification returned to the calling flow.
    let _ = session.transition(StreamSessionState::Failed);
}

// @cpt-end:cpt-cf-oagw-dod-sse-passthrough:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-websocket-upgrade:p1:inst-full
// The WebSocket upgrade contract of `cpt-cf-oagw-dod-websocket-upgrade`: the
// upgrade request runs every stage of the proxy-request pipeline exactly as
// any other request and the gateway establishes the upstream handshake against
// the resolved endpoint, verifies the upstream's `Sec-WebSocket-Accept`
// against the client's `Sec-WebSocket-Key` and relays 101 Switching
// Protocols; after the 101 the frames are pumped bidirectionally until either
// side closes, a Close frame received from one side is propagated to the
// other and no frame is buffered; every gateway-produced failure before the
// upgrade completes is `application/problem+json` through the closed table
// with `X-OAGW-Error-Source: gateway`, and the relayed 101 itself carries the
// gateway source; and a pump failure after the 101 is never re-serialised,
// both directions being closed and the failure recorded.

/// A duplex byte stream both ends of an upgraded session speak.
///
/// The upgraded connection is a byte stream of the RFC 6455 session: it is
/// relayed as bytes and nothing about a frame is read out of it.
pub trait DuplexIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> DuplexIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

/// The erased duplex connection an upgrade hands over, either end of the
/// session the pump relays.
pub type BoxDuplex = Box<dyn DuplexIo>;

/// The upgrade answer the bridge hands over: the 101 the upstream sent and the
/// two ends of the session it established, one of which the gateway relays to
/// the client and the other of which the upstream drives.
pub struct UpgradeHandover {
    /// The status the upstream sent, 101 for a verifiable upgrade.
    pub status: StatusCode,
    /// The handshake headers the RFC 6455 exchange produced.
    pub headers: HeaderMap,
    /// The gateway's end of the upgraded connection.
    pub io: BoxDuplex,
}

impl std::fmt::Debug for UpgradeHandover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeHandover")
            .field("status", &self.status.as_u16())
            .finish_non_exhaustive()
    }
}

/// One upgraded session's bidirectional pump.
///
/// The pump is the session of §4 after the 101 was relayed: it reads from one
/// side and writes to the other, never buffering a frame, never re-encoding
/// one and never interpreting an application frame's content. It is not
/// `Clone` and is never persisted; it ends with the connection it pumps.
pub struct TunnelPump {
    upstream: BoxDuplex,
    idle: Duration,
    session: StreamSession,
    journal: Arc<StreamJournal>,
    alias: String,
    endpoint_host: String,
}

impl TunnelPump {
    /// Builds the pump of one upgraded session, the idle interval it is bounded
    /// by being `proxy_timeout_secs`.
    #[must_use]
    pub fn new(
        upstream: BoxDuplex,
        idle: Duration,
        session: StreamSession,
        journal: Arc<StreamJournal>,
        alias: String,
        endpoint_host: String,
    ) -> Self {
        Self {
            upstream,
            idle,
            session,
            journal,
            alias,
            endpoint_host,
        }
    }
}

impl TunnelPump {
    /// The state the session the pump drives is in.
    ///
    /// The pump is the session of §4 after the 101 was relayed, and the state
    /// it reached is what a test of the streaming handover reads back; it is
    /// never a request's content and never a frame.
    #[must_use]
    pub fn session_state(&self) -> StreamSessionState {
        self.session.state()
    }

    /// Classifies the session the client half never established.
    ///
    /// The 101 of `inst-ws-13` had been written when the client's own upgrade
    /// half was abandoned, so the failure cannot be delivered any more: it is
    /// recorded as the abort it is and both directions are already closed.
    pub(crate) fn abort(&self) {
        record_failure(
            &self.journal,
            &self.session,
            &self.alias,
            &self.endpoint_host,
            StreamFailure::Aborted,
        );
    }
}

impl std::fmt::Debug for TunnelPump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No frame, no header value and no credential reaches a diagnostic.
        f.debug_struct("TunnelPump")
            .field("idle", &self.idle)
            .field("session", &self.session.state().as_str())
            .finish_non_exhaustive()
    }
}

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-14
// Pump frames bidirectionally: FOR EACH frame arriving from either direction,
// write it to the other side as it arrives, with no frame buffered, no frame
// re-encoded, and no interpretation of an application frame's content. The
// session is read as the RFC 6455 byte stream the upgrade established: the
// relay is a byte-level copy of exactly the chunk a read produced, so a Close
// frame, a Ping or a binary frame crosses as the bytes the upstream wrote.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-14

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-15
// Propagate the close: IF either side sends a Close frame or closes its
// connection, relay the Close frame to the other side when one was received
// and close both directions, so the session ends where its peer ended it.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-15

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-16
// IF the pump failed on one direction — a read or write error on an
// established session, or an idle interval of `proxy_timeout_secs` without a
// received frame on either side.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-16

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-17
// Close both directions without serialising a problem+json body onto the
// upgraded connection, the failure being one the client observes as the
// session ending, and classify it through
// `cpt-cf-oagw-algo-stream-failure-classification` — 502 `StreamAborted` for
// an aborted session, 504 `IdleTimeout` for an idle one — the mapping being
// recorded rather than delivered.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-17

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-18
// Close the `ProxyContext` in `Failed` and RETURN the classification to
// `cpt-cf-oagw-flow-proxy-request`.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-18

// @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-19
// ELSE the session ended by a peer's close: close both directions, close the
// `ProxyContext` in `Responded` and RETURN the session outcome to
// `cpt-cf-oagw-flow-proxy-request`, the pumped frames having originated at
// `cpt-cf-oagw-actor-upstream-service` even though the 101 relayed at
// `inst-ws-13` carries the gateway source per §1.5, and no frame ever having
// been buffered.
// @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-19

/// Pumps one upgraded session bidirectionally until either side closes it.
///
/// The relay is byte-level: every chunk a read produces is written to the
/// other side as it arrives, and nothing of the session is re-framed,
/// re-chunked, re-encoded or interpreted. An idle interval of `proxy_timeout_secs`
/// without a received byte on either side, and any read or write error, close
/// both directions and are recorded rather than delivered.
pub async fn pump(client: BoxDuplex, tunnel: TunnelPump) {
    let TunnelPump {
        upstream,
        idle,
        session,
        journal,
        alias,
        endpoint_host,
    } = tunnel;
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let mut from_client = vec![0u8; RELAY_CHUNK];
    let mut from_upstream = vec![0u8; RELAY_CHUNK];
    let outcome = loop {
        tokio::select! {
            read = tokio::time::timeout(idle, upstream_read.read(&mut from_upstream)) => {
                match read {
                    // `inst-ws-15`: the upstream closed the session; the Close
                    // frame it sent has already crossed as bytes, so the
                    // client's direction is closed behind it.
                    Ok(Ok(0)) => break PumpOutcome::Completed,
                    Ok(Ok(received)) => {
                        if client_write
                            .write_all(&from_upstream[..received])
                            .await
                            .is_err()
                        {
                            // `inst-ws-16`: the write into the client failed.
                            break PumpOutcome::Aborted;
                        }
                    }
                    Ok(Err(_)) => {
                        // `inst-ws-16`: the read of the upstream failed.
                        break PumpOutcome::Aborted;
                    }
                    Err(_elapsed) => {
                        // `inst-ws-16`: no frame received for the interval.
                        break PumpOutcome::Idle;
                    }
                }
            }
            read = tokio::time::timeout(idle, client_read.read(&mut from_client)) => {
                match read {
                    Ok(Ok(0)) => break PumpOutcome::Completed,
                    Ok(Ok(received)) => {
                        if upstream_write
                            .write_all(&from_client[..received])
                            .await
                            .is_err()
                        {
                            break PumpOutcome::Aborted;
                        }
                    }
                    Ok(Err(_)) => {
                        break PumpOutcome::Aborted;
                    }
                    Err(_elapsed) => {
                        break PumpOutcome::Idle;
                    }
                }
            }
        }
    };
    // Both directions are closed whichever way the session ended, the close
    // being the half of the lifecycle the flows name and the last chunk the
    // pump wrote having been flushed by the write that carried it.
    let _ = client_write.shutdown().await;
    let _ = upstream_write.shutdown().await;
    drop(client_read);
    drop(client_write);
    drop(upstream_read);
    drop(upstream_write);
    match outcome {
        PumpOutcome::Completed => {
            // `inst-ws-19`: the session ended by a peer's close, the context
            // closing in `Responded` and the outcome returning to the calling
            // flow.
            let _ = session.transition(StreamSessionState::Completed);
        }
        PumpOutcome::Aborted => {
            // `inst-ws-17` and `inst-ws-18`: the failure is classified and
            // recorded, never delivered, and the context closes in `Failed`.
            record_failure(
                &journal,
                &session,
                &alias,
                &endpoint_host,
                StreamFailure::Aborted,
            );
        }
        PumpOutcome::Idle => {
            // `inst-ws-17` and `inst-ws-18`: the idle interval of an
            // established session is the same classification the streamed body
            // reports.
            record_failure(
                &journal,
                &session,
                &alias,
                &endpoint_host,
                StreamFailure::Idle,
            );
        }
    }
}

/// The disposition the pump's close leaves the session in.
enum PumpOutcome {
    /// A peer closed the session: the session ends where its peer ended it.
    Completed,
    /// A read or a write of an established direction failed.
    Aborted,
    /// No byte was received from either side for the idle interval.
    Idle,
}
// @cpt-end:cpt-cf-oagw-dod-websocket-upgrade:p1:inst-full

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use bytes::Bytes;
    use http::{HeaderMap, HeaderValue, StatusCode};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::mpsc;

    use super::*;
    use crate::domain::error::OagwError;
    use http_body_util::BodyExt as _;

    use crate::domain::streaming::classify;
    use crate::infra::proxy::pipeline::{BoxError, OutboundBody, OutboundReply, StreamingBody};

    /// The idle interval the streamed-body and pump tests are bounded by, short
    /// enough that a test does not wait for a real timeout.
    const IDLE: Duration = Duration::from_millis(40);

    /// A streamed response the bridge hands over with no exact size hint.
    fn streamed_reply(
        content_type: Option<&str>,
        frames: mpsc::Receiver<Result<Bytes, BoxError>>,
    ) -> OutboundReply {
        let mut headers = HeaderMap::new();
        if let Some(content_type) = content_type {
            headers.insert(
                "content-type",
                HeaderValue::from_str(content_type).expect("the content type is a header value"),
            );
        }
        OutboundReply {
            status: StatusCode::OK,
            headers,
            body: OutboundBody::Streaming(StreamingBody::new(
                Box::pin(FrameStream { frames }),
                Duration::from_secs(30),
                hyper::body::SizeHint::default(),
            )),
        }
    }

    /// A buffered response the passthrough writes at once.
    fn buffered_reply(content_type: &str) -> OutboundReply {
        let mut headers = HeaderMap::new();
        headers.insert(
            "content-type",
            HeaderValue::from_str(content_type).expect("the content type is a header value"),
        );
        OutboundReply {
            status: StatusCode::OK,
            headers,
            body: OutboundBody::Full(Bytes::from_static(b"{\"ok\":true}")),
        }
    }

    /// Delivers the frames a test queued, ending when the sender is dropped.
    struct FrameStream {
        frames: mpsc::Receiver<Result<Bytes, BoxError>>,
    }

    impl futures_util::Stream for FrameStream {
        type Item = Result<Bytes, BoxError>;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Pin::new(&mut self.frames).poll_recv(cx)
        }
    }

    /// A duplex io whose reads never resolve, the idle branch of the pump
    /// needing a side that stays silent for the whole interval.
    struct SilentIo;

    impl AsyncRead for SilentIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for SilentIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A duplex io whose reads and writes fail at once, the abort branch of the
    /// pump needing a direction that reports a failure rather than an end.
    struct BrokenIo;

    impl AsyncRead for BrokenIo {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::other("the direction failed")))
        }
    }

    impl AsyncWrite for BrokenIo {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::other("the direction failed")))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// The test ends of the two duplex connections a pump is handed.
    struct Ends {
        client: tokio::io::DuplexStream,
        upstream: tokio::io::DuplexStream,
    }

    /// Builds the two duplex halves of a session, the pump ends handed over.
    fn ends() -> (Ends, BoxDuplex, BoxDuplex) {
        let (client_remote, client_local) = tokio::io::duplex(4096);
        let (upstream_remote, upstream_local) = tokio::io::duplex(4096);
        (
            Ends {
                client: client_remote,
                upstream: upstream_remote,
            },
            Box::new(client_local),
            Box::new(upstream_local),
        )
    }

    /// A session the 101 of `inst-ws-13` has already relayed: the pump is the
    /// stage after `Establishing`, so a pump test walks the declared states
    /// before it hands the session over.
    fn opened() -> StreamSession {
        let session = StreamSession::new();
        let _ = session.transition(StreamSessionState::Detected);
        let _ = session.transition(StreamSessionState::Establishing);
        let _ = session.transition(StreamSessionState::Streaming);
        session
    }

    /// The rows the journal recorded, oldest first.
    fn recorded(journal: &Arc<StreamJournal>) -> Vec<String> {
        journal
            .snapshot()
            .iter()
            .map(|event| event.row.clone())
            .collect()
    }

    #[test]
    fn an_sse_content_type_is_a_stream() {
        let (_tx, rx) = mpsc::channel(1);
        assert!(is_streamed(&streamed_reply(Some("text/event-stream"), rx)));
    }

    #[test]
    fn an_sse_content_type_with_parameters_is_a_stream() {
        let (_tx, rx) = mpsc::channel(1);
        assert!(is_streamed(&streamed_reply(
            Some("text/event-stream; charset=utf-8"),
            rx,
        )));
    }

    #[test]
    fn a_stream_without_an_exact_size_hint_is_a_stream() {
        let (_tx, rx) = mpsc::channel(1);
        assert!(is_streamed(&streamed_reply(Some("application/json"), rx)));
    }

    #[test]
    fn a_body_with_an_exact_size_hint_and_no_sse_type_is_buffered() {
        assert!(!is_streamed(&buffered_reply("application/json")));
    }

    #[test]
    fn the_stream_is_never_detected_on_the_request_side() {
        // The detection takes the response only: there is no request input at
        // all, which is what keeps an `Accept: text/event-stream` request from
        // making a response a stream.
        let mut headers = HeaderMap::new();
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
        assert!(!is_streamed(&OutboundReply {
            status: StatusCode::OK,
            headers,
            body: OutboundBody::Full(Bytes::from_static(b"{\"ok\":true}")),
        }));
    }

    #[test]
    fn the_journal_holds_the_capacity_and_drops_the_oldest() {
        let journal = StreamJournal::new();
        let session = StreamSession::new();
        for _ in 0..(JOURNAL_CAPACITY + 8) {
            record_failure(
                &journal,
                &session,
                "payments",
                "api.vendor.com",
                StreamFailure::Aborted,
            );
        }
        assert_eq!(journal.len(), JOURNAL_CAPACITY);
        let events = journal.snapshot();
        assert!(events.iter().all(|event| event.alias == "payments"));
        assert!(
            events
                .iter()
                .all(|event| event.endpoint_host == "api.vendor.com")
        );
    }

    #[tokio::test]
    async fn a_streamed_head_opens_the_session_through_the_declared_states() {
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let before = reply.headers.clone();
        let handed = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        assert_eq!(handed, Handover::Streamed);
        assert_eq!(session.state(), StreamSessionState::Streaming);
        assert_eq!(reply.headers, before);
    }

    #[tokio::test]
    async fn a_buffered_response_closes_the_session_as_completed() {
        let mut reply = buffered_reply("application/json");
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let handed = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        assert_eq!(handed, Handover::Buffered);
        assert_eq!(session.state(), StreamSessionState::Completed);
        assert!(journal.is_empty());
    }

    #[tokio::test]
    async fn an_aborted_stream_is_classified_stream_aborted() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Err(
            Box::new(io::Error::other("the upstream died")) as BoxError
        ))
        .await
        .expect("the frame is queued");
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        let failure = body
            .frame()
            .await
            .expect("the frame is delivered")
            .expect_err("the frame is the failure the test queued");
        drop(failure);
        assert_eq!(
            recorded(&journal),
            vec!["StreamAborted".to_owned()],
            "the abort is recorded as the row the closed table names"
        );
        assert_eq!(session.state(), StreamSessionState::Failed);
    }

    #[tokio::test]
    async fn an_idle_read_on_a_live_stream_is_classified_idle_timeout() {
        let (_tx, rx) = mpsc::channel(1);
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        let started = std::time::Instant::now();
        let frame = body.frame().await;
        let error = frame
            .expect("the idle clock ends the stream with the failure it carries")
            .expect_err("the idle clock raises the failure, it does not end quietly");
        drop(error);
        assert!(started.elapsed() >= IDLE);
        assert_eq!(recorded(&journal), vec!["IdleTimeout".to_owned()]);
        assert_eq!(session.state(), StreamSessionState::Failed);
    }

    #[tokio::test]
    async fn the_pump_relays_the_upstream_bytes_to_the_client_verbatim() {
        let (mut ends, client, upstream) = ends();
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        let pump = tokio::spawn(pump(
            client,
            TunnelPump::new(
                upstream,
                IDLE,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        ));
        // An RFC 6455 binary frame and a control byte: the pump copies the
        // bytes as they are, reading nothing out of them.
        let frame: [u8; 11] = [
            0x82, 0x05, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x00, 0xff, 0x0d, 0x0a,
        ];
        ends.upstream
            .write_all(&frame)
            .await
            .expect("the write lands");
        ends.upstream.flush().await.expect("the flush lands");
        let mut relayed = [0u8; 11];
        ends.client
            .read_exact(&mut relayed)
            .await
            .expect("the read lands");
        assert_eq!(relayed, frame);
        drop(ends.upstream);
        pump.await.expect("the pump ends with the session");
        assert!(journal.is_empty(), "a peer close records no failure");
        assert_eq!(session.state(), StreamSessionState::Completed);
    }

    #[tokio::test]
    async fn the_pump_relays_the_client_bytes_to_the_upstream_verbatim() {
        let (mut ends, client, upstream) = ends();
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        let pump = tokio::spawn(pump(
            client,
            TunnelPump::new(
                upstream,
                IDLE,
                session,
                journal,
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        ));
        // An RFC 6455 Close frame as the client sent it.
        let frame: [u8; 4] = [0x88, 0x02, 0x03, 0xe8];
        ends.client
            .write_all(&frame)
            .await
            .expect("the write lands");
        ends.client.flush().await.expect("the flush lands");
        let mut relayed = [0u8; 4];
        ends.upstream
            .read_exact(&mut relayed)
            .await
            .expect("the read lands");
        assert_eq!(relayed, frame);
        drop(ends.client);
        pump.await.expect("the pump ends with the session");
    }

    #[tokio::test]
    async fn a_peer_close_ends_the_session_where_its_peer_ended_it() {
        let (mut ends, client, upstream) = ends();
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        let pump = tokio::spawn(pump(
            client,
            TunnelPump::new(
                upstream,
                IDLE,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        ));
        drop(ends.client);
        pump.await.expect("the pump ends with the session");
        assert_eq!(session.state(), StreamSessionState::Completed);
        assert!(journal.is_empty());
        // The other direction is closed behind the peer's close.
        let mut silence = [0u8; 4];
        assert_eq!(ends.upstream.read(&mut silence).await.expect("eof"), 0);
    }

    #[tokio::test]
    async fn a_pump_direction_failure_is_classified_stream_aborted() {
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        pump(
            Box::new(BrokenIo),
            TunnelPump::new(
                Box::new(SilentIo),
                IDLE,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        )
        .await;
        assert_eq!(recorded(&journal), vec!["StreamAborted".to_owned()]);
        assert_eq!(session.state(), StreamSessionState::Failed);
    }

    #[tokio::test]
    async fn an_idle_session_is_classified_idle_timeout() {
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        let started = std::time::Instant::now();
        pump(
            Box::new(SilentIo),
            TunnelPump::new(
                Box::new(SilentIo),
                IDLE,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        )
        .await;
        assert!(started.elapsed() >= IDLE);
        assert_eq!(recorded(&journal), vec!["IdleTimeout".to_owned()]);
        assert_eq!(session.state(), StreamSessionState::Failed);
    }

    #[test]
    fn the_rows_the_pump_records_are_the_closed_table_rows() {
        for (failure, variant) in [
            (StreamFailure::Aborted, "StreamAborted"),
            (StreamFailure::Idle, "IdleTimeout"),
        ] {
            let outcome = classify(failure, true);
            assert_eq!(outcome.row.mapping().variant, variant);
            assert!(!outcome.deliverable);
        }
    }

    #[tokio::test]
    async fn a_streamed_body_forwards_each_frame_as_the_upstream_produces_it() {
        let (tx, rx) = mpsc::channel(1);
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let head = reply.headers.clone();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        // The first frame is read before the second is produced: the relay
        // waits for no frame and accumulates none.
        let first = Bytes::from_static(b"data: one\n\n");
        tx.send(Ok(first.clone()))
            .await
            .expect("the frame is queued");
        let frame = body
            .frame()
            .await
            .expect("the frame is delivered")
            .expect("the frame is a data frame");
        assert_eq!(
            frame.into_data().expect("a data frame").as_ref(),
            &first[..]
        );
        for index in 2..=4 {
            let queued = Bytes::from(format!("data: {index}\n\n"));
            tx.send(Ok(queued.clone()))
                .await
                .expect("the frame is queued");
            let frame = body
                .frame()
                .await
                .expect("the frame is delivered")
                .expect("the frame is a data frame");
            assert_eq!(
                frame.into_data().expect("a data frame").as_ref(),
                &queued[..]
            );
        }
        // The head is relayed exactly as the upstream produced it.
        assert_eq!(reply.headers, head);
        assert_eq!(
            reply.headers.get("content-type").expect("the head"),
            "text/event-stream"
        );
        // The upstream ended the body: the stream completes and records nothing.
        drop(tx);
        assert!(body.frame().await.is_none());
        assert_eq!(session.state(), StreamSessionState::Streaming);
        assert!(journal.is_empty());
    }

    #[tokio::test]
    async fn a_non_sse_stream_is_forwarded_incrementally_too() {
        // The bridge handed the body over as a stream, so it is forwarded frame
        // by frame although its content type is not the SSE one.
        let (tx, rx) = mpsc::channel(1);
        let mut reply = streamed_reply(Some("application/vnd.vendor.v1+json"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        assert_eq!(
            handover(
                &mut reply,
                &session,
                &journal,
                "payments",
                "api.vendor.com",
                IDLE,
            ),
            Handover::Streamed
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        tx.send(Ok(Bytes::from_static(b"{\"partial\":")))
            .await
            .expect("the frame is queued");
        let first = body
            .frame()
            .await
            .expect("the frame is delivered")
            .expect("the frame is a data frame");
        assert_eq!(
            first.into_data().expect("a data frame").as_ref(),
            &b"{\"partial\":"[..]
        );
        // Read before the second frame is produced: nothing is buffered.
        tx.send(Ok(Bytes::from_static(b"1}")))
            .await
            .expect("the frame is queued");
        let second = body
            .frame()
            .await
            .expect("the frame is delivered")
            .expect("the frame is a data frame");
        assert_eq!(
            second.into_data().expect("a data frame").as_ref(),
            &b"1}"[..]
        );
        assert_eq!(session.state(), StreamSessionState::Streaming);
        assert!(journal.is_empty());
    }

    #[tokio::test]
    async fn a_live_stream_that_keeps_producing_bytes_is_never_ended() {
        let (tx, rx) = mpsc::channel(1);
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        let started = std::time::Instant::now();
        for index in 0..5 {
            // Half an idle interval between two received bytes, so no interval
            // without one reaches the configured bound.
            tokio::time::sleep(IDLE / 2).await;
            tx.send(Ok(Bytes::from(format!("data: {index}\n\n"))))
                .await
                .expect("the frame is queued");
            let frame = body
                .frame()
                .await
                .expect("the live frame is delivered")
                .expect("the frame is a data frame");
            drop(frame);
        }
        // The stream has now lived longer than one idle interval and is still
        // open: the configured value bounds the interval without a received
        // byte, not the total duration of the stream.
        assert!(started.elapsed() > IDLE + IDLE, "{:?}", started.elapsed());
        assert_eq!(journal.len(), 0, "no failure was classified");
        assert_eq!(session.state(), StreamSessionState::Streaming);
    }

    #[tokio::test]
    async fn a_close_frame_from_the_upstream_crosses_to_the_client_verbatim() {
        let (mut ends, client, upstream) = ends();
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        let pump = tokio::spawn(pump(
            client,
            TunnelPump::new(
                upstream,
                IDLE,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        ));
        // An RFC 6455 Close frame carrying a status code and a reason, as the
        // upstream wrote it.
        let close: [u8; 6] = [0x88, 0x04, 0x03, 0xe8, 0x62, 0x79];
        ends.upstream
            .write_all(&close)
            .await
            .expect("the write lands");
        ends.upstream.flush().await.expect("the flush lands");
        let mut relayed = [0u8; 6];
        ends.client
            .read_exact(&mut relayed)
            .await
            .expect("the read lands");
        assert_eq!(
            relayed, close,
            "the Close frame crossed as the bytes the upstream wrote"
        );
        drop(ends.upstream);
        pump.await.expect("the pump ends with the session");
        assert_eq!(session.state(), StreamSessionState::Completed);
        assert!(journal.is_empty(), "a peer close records no failure");
    }

    #[tokio::test]
    async fn a_pump_failure_writes_no_problem_json_onto_the_upgraded_connection() {
        let (mut ends, client, _upstream) = ends();
        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        // The upstream direction of the established session fails at once.
        pump(
            client,
            TunnelPump::new(
                Box::new(BrokenIo),
                IDLE,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        )
        .await;
        assert_eq!(recorded(&journal), vec!["StreamAborted".to_owned()]);
        assert_eq!(session.state(), StreamSessionState::Failed);
        // Both directions are closed and nothing was serialised onto the
        // upgraded connection: the caller observes the session ending.
        let mut relayed = Vec::new();
        let read = ends
            .client
            .read_to_end(&mut relayed)
            .await
            .expect("the read ends");
        assert_eq!(read, 0, "the connection is closed, not answered");
        assert!(
            relayed.is_empty(),
            "no problem+json was written: {:?}",
            String::from_utf8_lossy(&relayed)
        );
    }

    #[tokio::test]
    async fn one_configured_interval_bounds_both_the_stream_and_the_session() {
        // `proxy_timeout_secs` is read once: the value the head relay re-times
        // the streamed body with is the one the pump bounds the session by, and
        // neither is the bridge default the body was handed over with.
        let configured = Duration::from_millis(120);
        let (_tx, rx) = mpsc::channel(1);
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            configured,
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        let started = std::time::Instant::now();
        let frame = body.frame().await;
        drop(frame);
        assert!(
            started.elapsed() >= configured,
            "the streamed body is bounded by the configured interval"
        );
        assert_eq!(recorded(&journal), vec!["IdleTimeout".to_owned()]);

        let journal = Arc::new(StreamJournal::new());
        let session = opened();
        let started = std::time::Instant::now();
        pump(
            Box::new(SilentIo),
            TunnelPump::new(
                Box::new(SilentIo),
                configured,
                session.clone(),
                Arc::clone(&journal),
                "payments".to_owned(),
                "api.vendor.com".to_owned(),
            ),
        )
        .await;
        assert!(
            started.elapsed() >= configured,
            "the session is bounded by the same configured interval"
        );
        assert_eq!(recorded(&journal), vec!["IdleTimeout".to_owned()]);
        assert_eq!(session.state(), StreamSessionState::Failed);
    }

    #[tokio::test]
    async fn a_recorded_stream_failure_carries_no_frame_content() {
        let (tx, rx) = mpsc::channel(1);
        // The frame content is the credential-bearing material the redaction
        // rules keep out of every record the feature produces.
        let frame = Bytes::from_static(b"Bearer abc.def cred://payments-key ?token=secret");
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        // The channel holds one frame at a time, so the data frame is relayed
        // before the failing one is queued.
        tx.send(Ok(frame.clone()))
            .await
            .expect("the frame is queued");
        let _ = body.frame().await.expect("the frame is delivered");
        tx.send(Err(
            Box::new(io::Error::other("the upstream died")) as BoxError
        ))
        .await
        .expect("the failing frame is queued");
        let _ = body.frame().await;
        let events = journal.snapshot();
        assert_eq!(events.len(), 1, "one classification for one failure");
        let event = &events[0];
        assert_eq!(event.alias, "payments");
        assert_eq!(event.endpoint_host, "api.vendor.com");
        assert_eq!(event.row, "StreamAborted");
        assert_eq!(event.state, StreamSessionState::Streaming);
        let rendered = format!("{event:?}");
        for carried in ["Bearer", "cred://", "token="] {
            assert!(
                !rendered.contains(carried),
                "the classification carries {carried:?}: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn a_streamed_failure_is_classified_exactly_once() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(Err(
            Box::new(io::Error::other("the upstream died")) as BoxError
        ))
        .await
        .expect("the failing frame is queued");
        let mut reply = streamed_reply(Some("text/event-stream"), rx);
        let journal = Arc::new(StreamJournal::new());
        let session = StreamSession::new();
        let _ = handover(
            &mut reply,
            &session,
            &journal,
            "payments",
            "api.vendor.com",
            IDLE,
        );
        // The classification is reported to the journal the stage owns and to
        // the emitter the emission path reads, and reaches neither twice.
        let emissions = Arc::new(Mutex::new(Vec::new()));
        let OutboundBody::Streaming(body) = &mut reply.body else {
            panic!("the streamed reply carries a streaming body");
        };
        let reported = Arc::clone(&emissions);
        body.set_stream_emitter(Arc::new(move |failure| {
            reported.lock().push(failure);
        }));
        let failed = body.frame().await;
        drop(failed);
        assert_eq!(journal.len(), 1, "one classification, one record");
        assert_eq!(session.state(), StreamSessionState::Failed);
        assert_eq!(*emissions.lock(), vec![StreamFailure::Aborted]);
    }

    #[test]
    fn the_streamed_rows_carry_their_closed_detail_and_status() {
        let aborted = classify(StreamFailure::Aborted, false);
        let idle = classify(StreamFailure::Idle, false);
        assert_eq!(aborted.row.mapping().status, 502);
        assert_eq!(idle.row.mapping().status, 504);
        assert!(aborted.deliverable);
        assert!(idle.deliverable);
        assert_eq!(
            aborted.row.mapping().variant,
            OagwError::stream_aborted("the stream aborted")
                .mapping()
                .variant,
            "the aborted row is the closed StreamAborted row"
        );
        assert_eq!(
            idle.row.mapping().variant,
            OagwError::idle_timeout("the stream went idle")
                .mapping()
                .variant,
            "the idle row is the closed IdleTimeout row"
        );
        assert!(!aborted.row.detail().is_empty());
        assert!(!idle.row.detail().is_empty());
    }
}
