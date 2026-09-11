//! The pure streaming decisions of `cpt-cf-oagw-feature-streaming-proxy`.
//!
//! The module holds the three building blocks FEATURE §3 declares and the
//! per-stream session machine of §4: the upgrade-header carve-out, the stream
//! failure classification and the RFC 6455 `Sec-WebSocket-Accept` reading the
//! handshake is verified with. Everything here is pure: no I/O, no clock, no
//! network, no frame content. The inputs are the request headers the pipeline
//! classified, the selected endpoint's scheme and the failure a streamed
//! connection produced; the outputs are the decisions the pipeline and the
//! streaming stage in `crate::infra::proxy::streaming` act on.
//!
//! The module never re-declares a strip list, a validation rule, an error
//! mapping or a configuration key owned by another feature: the rows it maps
//! onto are the closed table of `crate::domain::error`, the strip list it
//! exempts from is the base pipeline's `HOP_BY_HOP_HEADERS`, and the one
//! timeout value it is described by is the `OagwConfig` key the gear-wiring
//! feature owns.
// @cpt-begin:cpt-cf-oagw-dod-stream-timeout-and-error-source:p1:inst-full
// The timeout and error-source contract of
// `cpt-cf-oagw-dod-stream-timeout-and-error-source`: `proxy_timeout_secs`
// bounds BOTH the establishment of a streamed connection and the idle-read
// interval of an established one, no second key is read and no streaming key
// exists; an idle read on a live stream maps onto 504 `IdleTimeout`, an abort
// onto 502 `StreamAborted`, an elapsed establishment onto 504
// `ConnectionTimeout`, an unreachable endpoint onto 503 `LinkUnavailable` and
// a non-verifiable handshake answer onto 502 `ProtocolError`; a failure whose
// head or 101 was not yet relayed is deliverable as problem+json with
// `X-OAGW-Error-Source: gateway`, and a failure whose head or 101 was relayed
// is not deliverable at all — it is recorded and it ends the connection.

use std::sync::Arc;

use base64::Engine as _;
use http::HeaderMap;
use parking_lot::Mutex;

use crate::domain::error::OagwError;
use crate::domain::model::EndpointScheme;
use crate::domain::proxy::SEC_WEBSOCKET_PREFIX;

/// The `Upgrade` request header whose value names the upgrade protocol.
const UPGRADE_HEADER: &str = "upgrade";

/// The token an `Upgrade` header carries for an RFC 6455 handshake.
const WEBSOCKET_TOKEN: &str = "websocket";

/// The `Connection` header the hop-by-hop strip list also consumes.
const CONNECTION_HEADER: &str = "connection";

/// The RFC 6455 GUID the `Sec-WebSocket-Accept` value is derived from
/// (RFC 6455 §1.3), spelled in the handshake grammar the protocol defines.
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The detail the idle-read classification carries, a static description that
/// names no header value, no query string, no credential and no frame content.
const IDLE_DETAIL: &str = "stream idle read reached the proxy timeout";

/// The detail the abort classification carries, a static description for the
/// same reason.
const ABORTED_DETAIL: &str = "upstream stream ended before the upstream ended it";

/// The detail the establishment classification carries, a static description
/// for the same reason.
const ESTABLISH_TIMEOUT_DETAIL: &str = "stream establishment reached the proxy timeout";

/// The detail the unreachable-endpoint classification carries, a static
/// description for the same reason.
const UNREACHABLE_DETAIL: &str = "stream endpoint unreachable";

/// The detail the protocol classification carries, a static description for
/// the same reason.
const PROTOCOL_DETAIL: &str = "stream handshake answer not a verifiable upgrade";

/// A failure an established stream or an established session produced.
///
/// The establishment variants exist so a streaming establishment failure and a
/// base-pipeline transport failure can be shown to map onto the same rows; a
/// streamed failure is never invented outside the cases the flows of §2 name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamFailure {
    /// An idle interval of `proxy_timeout_secs` without a received byte or
    /// frame on an established stream or session.
    Idle,
    /// The upstream connection aborted mid-stream, or one direction of an
    /// established pump failed.
    Aborted,
    /// The connection or the upgrade was still being established when
    /// `proxy_timeout_secs` elapsed.
    EstablishTimedOut,
    /// The connection or the upgrade was still being established and the
    /// endpoint was unreachable or refused the connection.
    EstablishUnreachable,
    /// The connection or the upgrade was still being established and the
    /// answer received was not a verifiable upgrade answer.
    EstablishAnswered,
    /// A protocol failure of an attempted exchange.
    Protocol,
}

/// The classification of one streaming failure.
#[derive(Debug)]
pub struct StreamOutcome {
    /// The closed-table row the failure maps onto.
    pub row: OagwError,
    /// Whether a problem+json body could still be delivered, decided from the
    /// relayed flag: a failure whose head or 101 was already relayed is never
    /// delivered, it only ends the connection.
    pub deliverable: bool,
}

// @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-01
// Take the failure and the relayed flag as the inputs, and the closed 22-row
// table of `cpt-cf-oagw-algo-error-mapping` as the only vocabulary: no row is
// added, no row is renamed, and the mapping the base pipeline performs at
// `inst-pf-34` is the same mapping this algorithm consults.
// @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-01
///
/// The output is the row, the connection disposition and whether a problem+json
/// body could still be delivered.
///
/// # Errors
///
/// Never returns an error: the function builds the row it returns, and the
/// `Result`-free signature is the contract that one failure produces exactly
/// one classification (`inst-fc-13`).
pub fn classify(failure: StreamFailure, relayed: bool) -> StreamOutcome {
    let row = match failure {
        StreamFailure::Idle => {
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-02
            // IF the failure is an idle interval of `proxy_timeout_secs`
            // without a received byte or frame on an established stream or
            // session.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-02
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-03
            // Map onto the existing `IdleTimeout` row — HTTP 504, GTS type
            // `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`, retriable
            // `Yes` — the row the DECOMPOSITION entry's timeout bullet names.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-03
            OagwError::idle_timeout(IDLE_DETAIL)
        }
        StreamFailure::Aborted => {
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-04
            // ELSE IF the upstream connection aborted mid-stream, or one
            // direction of an established pump failed.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-04
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-05
            // Map onto the existing `StreamAborted` row — HTTP 502, GTS type
            // `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`, retriable
            // `No` — and never onto `DownstreamError`, whose assignment
            // belongs to the base branch's own mapping and which this feature
            // never produces.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-05
            OagwError::stream_aborted(ABORTED_DETAIL)
        }
        StreamFailure::EstablishTimedOut | StreamFailure::EstablishUnreachable => {
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-06
            // ELSE IF the failure happened while the connection or the upgrade
            // was still being established.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-06
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-07
            // Map onto the existing rows the base pipeline already maps
            // establishment failures onto: 504 `ConnectionTimeout` for an
            // elapsed `proxy_timeout_secs`, 503 `LinkUnavailable` for an
            // unreachable or refused endpoint, 502 `ProtocolError` for a
            // handshake answer that is not a verifiable 101.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-07
            match failure {
                StreamFailure::EstablishTimedOut => {
                    OagwError::connection_timeout(ESTABLISH_TIMEOUT_DETAIL)
                }
                _ => OagwError::link_unavailable(UNREACHABLE_DETAIL),
            }
        }
        // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-08
        // ELSE the failure is a protocol failure of an attempted exchange.
        // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-08
        StreamFailure::EstablishAnswered | StreamFailure::Protocol => {
            // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-09
            // Map onto the existing `ProtocolError` row — HTTP 502 — and never
            // invent a streaming-specific variant, a streaming-specific status
            // or a streaming-specific GTS type.
            // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-09
            OagwError::protocol_error(PROTOCOL_DETAIL)
        }
    };
    // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-10
    // Decide the delivery surface from the relayed flag: IF nothing of the
    // streamed response and nothing of the upgrade had been relayed yet, the
    // row is rendered by `cpt-cf-oagw-flow-error-response` of the gear-wiring
    // feature with `X-OAGW-Error-Source: gateway` (`inst-fc-11`), the
    // rendering being that feature's and this algorithm supplying only the row.
    // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-10
    // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-11
    // The deliverable half of the decision: the caller renders the row as
    // `application/problem+json` with the gateway source, the rendering being
    // that feature's and this algorithm supplying only the row.
    // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-11
    // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-12
    // ELSE deliver nothing: close the upstream connection and the client
    // connection, record the mapped row for the instruments the observability
    // feature emits — `oagw_errors_total` with the mapped `error_type` — and
    // let the caller observe the stream or session ending, no problem+json
    // being written onto a connection whose head or 101 was already relayed.
    // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-12
    let deliverable = !relayed;
    // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-13
    // Never retry: the failure produces exactly one classification, no second
    // upstream attempt, no re-issue of the client request and no gateway-side
    // reconnect, per `cpt-cf-oagw-principle-no-retry`.
    // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-13
    // @cpt-begin:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-14
    // RETURN the mapped row, the connection disposition and the delivery
    // surface to the calling flow — `cpt-cf-oagw-flow-sse-passthrough` or
    // `cpt-cf-oagw-flow-websocket-upgrade`.
    // @cpt-end:cpt-cf-oagw-algo-stream-failure-classification:p1:inst-fc-14
    StreamOutcome { row, deliverable }
}

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-01
// Take the header set, the `Upgrade` header value and the upgrade flag as the
// inputs; the strip list itself — `Connection`, `Keep-Alive`,
// `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`,
// `Transfer-Encoding`, `Upgrade` — is the base pipeline's at `inst-pf-20` and
// is not restated, re-owned or extended here.
//
// The function below answers one header-name question at a time and is applied
// once per request, by the header-processing stage of
// `cpt-cf-oagw-flow-proxy-request` for a WebSocket upgrade request, its
// applied exemption being what
// `cpt-cf-oagw-flow-websocket-upgrade` confirms at `inst-ws-03`.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-01

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-02
// IF the request is not a WebSocket upgrade.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-02

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-03
// Apply the strip list unconditionally, exempting nothing, and RETURN the
// stripped header set: the carve-out is scoped to WebSocket upgrade requests
// only and has no existence outside them, so an SSE request and every other
// proxied request are stripped exactly as `inst-pf-20` strips them. The
// `is_upgrade` flag being `false` answers `false` for every name, which is
// that unconditional application read at the granularity of one header.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-03

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-04
// ELSE exempt exactly three header families from the strip list: `Upgrade`,
// `Connection` and the `Sec-WebSocket-*` family, the last not being a
// strip-list member at all and therefore needing only to be kept.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-04

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-05
// Keep `Upgrade` so the upstream sees the upgrade token, keep `Connection` so
// the upgrade's connection-level tokens survive, and keep every header whose
// name begins with `Sec-WebSocket-`, of which `Sec-WebSocket-Key`,
// `Sec-WebSocket-Version`, `Sec-WebSocket-Protocol` and
// `Sec-WebSocket-Extensions` are the ones the RFC 6455 exchange uses.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-05

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-06
// Strip every other member of the list — `Keep-Alive`,
// `Proxy-Authenticate`, `Proxy-Authorization`, `TE`, `Trailer`,
// `Transfer-Encoding` — with no second exemption, no general upgrade exemption
// and no exemption for any header outside the list, and leave the `request.*`
// rules of the selected upstream's `upstream.headers` block to be applied
// afterwards in the order the pipeline applies them.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-06

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-07
// The exemption is a request-side rule only: it carves the outbound request
// headers of an upgrade and never the response headers of the 101 relayed at
// `inst-ws-13`, which are relayed as the RFC 6455 exchange produced them.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-07

// @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-08
// RETURN the outbound header set to the caller — the header-processing stage
// of `cpt-cf-oagw-flow-proxy-request` for an upgrade request, or
// `cpt-cf-oagw-flow-websocket-upgrade` at `inst-ws-03`.
// @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-08
///
/// Reports whether the header-processing stage forwards an inbound header
/// named `name`, given whether the request is a WebSocket upgrade.
///
/// The header name arrives lower-cased from a `HeaderMap` iteration, which is
/// why the comparison is ASCII case-insensitive rather than exact.
#[must_use]
pub fn carve_out_keeps(name: &str, is_upgrade: bool) -> bool {
    if !is_upgrade {
        return false;
    }
    name.eq_ignore_ascii_case(UPGRADE_HEADER)
        || name.eq_ignore_ascii_case(CONNECTION_HEADER)
        || name
            .get(..SEC_WEBSOCKET_PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SEC_WEBSOCKET_PREFIX))
}

/// Reports whether a client request carries `Upgrade: websocket`
/// (`inst-ws-02`), the one detection this feature performs on the request side.
///
/// The detection is a value test and never a classification of a response: an
/// `Accept: text/event-stream` request header alone never makes a response a
/// stream, and an `Upgrade` header whose value names another protocol is not
/// this feature's handshake.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get_all(UPGRADE_HEADER)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case(WEBSOCKET_TOKEN))
        })
}

/// Reports whether the scheme the endpoint selection returned can carry a
/// WebSocket upgrade (`inst-ws-05` to `inst-ws-07`).
///
/// `wss` is eligible unconditionally. `http` is eligible because a request
/// that reached the streaming stage already passed the scheme policy of
/// `inst-pf-30` and `inst-pf-31`, which is where an `http` endpoint with
/// `allow_http_upstream` `false` is refused with 400 `ValidationError` — no
/// row is added here and no header is carved out for a request refused there.
/// An endpoint whose scheme is `https` but not `wss`, a `grpc` endpoint and a
/// `wt` endpoint are refused here, before any handshake is attempted.
///
/// # Errors
///
/// Returns 400 `RouteError` through the existing row for a scheme the §1.5
/// reading cannot carry an upgrade on, and never invents a streaming-specific
/// row.
pub fn upgrade_scheme_eligible(scheme: EndpointScheme) -> Result<(), OagwError> {
    match scheme {
        EndpointScheme::Wss | EndpointScheme::Http => Ok(()),
        EndpointScheme::Https | EndpointScheme::Grpc | EndpointScheme::Wt => Err(
            OagwError::route_error("endpoint scheme cannot carry a websocket upgrade"),
        ),
    }
}

/// Derives the `Sec-WebSocket-Accept` value the RFC 6455 handshake requires
/// (RFC 6455 §1.3): base64 of the SHA-1 of the concatenation of the client's
/// `Sec-WebSocket-Key` and the protocol GUID.
///
/// The digest is computed inline so the handshake needs no hashing
/// dependency, and the derivation never reads, logs or stores the key's
/// provenance beyond the key the handshake itself carries.
#[must_use]
pub fn websocket_accept(key: &str) -> String {
    let mut digest_input = String::with_capacity(key.len() + WEBSOCKET_GUID.len());
    digest_input.push_str(key);
    digest_input.push_str(WEBSOCKET_GUID);
    base64::engine::general_purpose::STANDARD.encode(sha1(digest_input.as_bytes()))
}

/// Verifies the upstream's `Sec-WebSocket-Accept` against the client's
/// `Sec-WebSocket-Key` (`inst-ws-09`), the verification being a byte
/// comparison of the derived value with the one the upstream answered.
///
/// An accept that does not verify is a protocol failure and nothing is relayed.
#[must_use]
pub fn verify_websocket_accept(key: &str, accept: &str) -> bool {
    websocket_accept(key) == accept.trim()
}

/// Computes the SHA-1 digest of `message` (RFC 3174).
///
/// The handshake is the only consumer, which is why the digest is computed
/// here rather than a hashing crate being pulled in for twenty lines of
/// padding and eighty rounds.
fn sha1(message: &[u8]) -> [u8; 20] {
    let mut digest = [0u8; 20];
    let mut state = [
        0x6745_2301u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = u64::try_from(message.len()).map_or(0, |len| len.wrapping_mul(8));
    let mut padded = Vec::with_capacity(message.len() + 72);
    padded.extend_from_slice(message);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut words = [0u32; 80];
        for (index, word) in block.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            let mixed = words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16];
            words[index] = mixed.rotate_left(1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        for (round, word) in words.iter().take(80).enumerate() {
            let mixed = round_mix(round, a, b, c, d, e, *word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = mixed;
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

/// One SHA-1 round: the round function and constant of `round` applied over
/// the five state words and the schedule word the round consumes.
fn round_mix(round: usize, a: u32, b: u32, c: u32, d: u32, e: u32, word: u32) -> u32 {
    let (function, constant) = match round {
        0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
        20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
        40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
        _ => (b ^ c ^ d, 0xCA62_C1D6),
    };
    a.rotate_left(5)
        .wrapping_add(function)
        .wrapping_add(e)
        .wrapping_add(constant)
        .wrapping_add(word)
}

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-01
// FROM HandedOver TO Detected WHEN the upstream response was classified a
// stream at `inst-sse-03`, or the client request was classified an upgrade at
// `inst-ws-02`.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-01

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-02
// FROM HandedOver TO Completed WHEN neither classification matched: the
// response is the buffered passthrough of `inst-pf-37` and this machine never
// opened a stream.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-02

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-03
// FROM Detected TO Establishing WHEN the endpoint-scheme reading of §1.5
// allowed the endpoint and the handshake of `inst-ws-09` began, an SSE stream
// entering this state only as it reaches the head relay, which is the whole
// establishment an SSE stream performs.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-03

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-04
// FROM Detected TO Failed WHEN the scheme reading refused the endpoint at
// `inst-ws-07` — no handshake attempted and the rejection rendered as
// problem+json.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-04

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-05
// FROM Establishing TO Streaming WHEN the 101 was relayed at `inst-ws-13`, or
// the streamed head was relayed at `inst-sse-05`.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-05

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-06
// FROM Establishing TO Failed WHEN the handshake failed at `inst-ws-11` —
// `proxy_timeout_secs` elapsed, the answer was not a 101, the
// `Sec-WebSocket-Accept` did not verify, or the endpoint was unreachable — the
// failure being rendered as problem+json because nothing was relayed.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-06

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-07
// FROM Streaming TO Completed WHEN the upstream ended the SSE body at
// `inst-sse-14`, or either peer closed the session at `inst-ws-15`.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-07

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-08
// FROM Streaming TO Failed WHEN an idle read reached `proxy_timeout_secs` at
// `inst-sse-08`, the upstream connection aborted at `inst-sse-12`, or a pump
// direction failed at `inst-ws-17`, the mapped row being recorded and both
// directions closed.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-08

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-09
// FROM Completed TO HandedOver WHEN the next streamed request is handed over:
// the machine is the lifecycle of one stream and holds no state between
// streams.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-09

// @cpt-begin:cpt-cf-oagw-state-stream-session:p1:inst-ss-10
// FROM Failed TO HandedOver WHEN the next streamed request is handed over, the
// classification having been recorded and the context discarded with the
// request that produced it.
// @cpt-end:cpt-cf-oagw-state-stream-session:p1:inst-ss-10

/// The per-stream lifecycle of one streamed request
/// (`cpt-cf-oagw-state-stream-session`).
///
/// The machine holds no state between streams, is never persisted and has no
/// cache behind it: every streamed request opens a fresh `HandedOver` session
/// and the only transitions out of `Completed` and `Failed` belong to the next
/// one. Both are terminal within one stream, and no state other than them is
/// observable from outside the gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamSessionState {
    /// The streamed request was handed over and no classification has run yet.
    HandedOver,
    /// The upstream response was classified a stream, or the client request an
    /// upgrade.
    Detected,
    /// The handshake of an upgrade, or the head relay of a stream, is running.
    Establishing,
    /// The head or the 101 was relayed and the stream or session is live.
    Streaming,
    /// The upstream ended the body, or a peer closed the session.
    Completed,
    /// A failure was classified after the detection, and both directions were
    /// closed.
    Failed,
}

impl StreamSessionState {
    /// The name of the state, for diagnostics and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HandedOver => "HandedOver",
            Self::Detected => "Detected",
            Self::Establishing => "Establishing",
            Self::Streaming => "Streaming",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
        }
    }
}

/// One streamed request's session.
///
/// The session is shared between the request path that opens it and the stage
/// that closes it — the streamed body's failure recorder, or the bidirectional
/// pump — so its state sits behind a lock and the session is cloned, never
/// persisted and never held past the request that produced it.
#[derive(Debug, Clone)]
pub struct StreamSession {
    state: Arc<Mutex<StreamSessionState>>,
}

impl StreamSession {
    /// Opens a fresh session in `HandedOver`, the state every streamed request
    /// begins in and the only state a completed or failed session returns to.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(StreamSessionState::HandedOver)),
        }
    }

    /// The state the session is in.
    #[must_use]
    pub fn state(&self) -> StreamSessionState {
        *self.state.lock()
    }

    /// Applies the closed transition set of FEATURE §4.
    ///
    /// Any transition not listed is invalid and leaves the session unchanged,
    /// so the session can never reach a state no stage produced. The ordered
    /// stages that share the session cannot propose one, which is why a
    /// rejected transition is never a request failure.
    ///
    /// # Errors
    ///
    /// Returns 400 `ValidationError` through the existing row when the
    /// transition is not one of the ten the machine declares, the session then
    /// being left in the state it was in.
    pub fn transition(&self, to: StreamSessionState) -> Result<(), OagwError> {
        let current = *self.state.lock();
        let valid = matches!(
            (current, to),
            (StreamSessionState::HandedOver, StreamSessionState::Detected)
                | (
                    StreamSessionState::HandedOver,
                    StreamSessionState::Completed
                )
                | (
                    StreamSessionState::Detected,
                    StreamSessionState::Establishing
                )
                | (StreamSessionState::Detected, StreamSessionState::Failed)
                | (
                    StreamSessionState::Establishing,
                    StreamSessionState::Streaming
                )
                | (StreamSessionState::Establishing, StreamSessionState::Failed)
                | (StreamSessionState::Streaming, StreamSessionState::Completed)
                | (StreamSessionState::Streaming, StreamSessionState::Failed)
                | (
                    StreamSessionState::Completed,
                    StreamSessionState::HandedOver
                )
                | (StreamSessionState::Failed, StreamSessionState::HandedOver)
        );
        if valid {
            *self.state.lock() = to;
            Ok(())
        } else {
            Err(OagwError::validation_error(format!(
                "stream session cannot transition from {} to {}",
                current.as_str(),
                to.as_str()
            )))
        }
    }
}

impl Default for StreamSession {
    fn default() -> Self {
        Self::new()
    }
}
// @cpt-end:cpt-cf-oagw-dod-stream-timeout-and-error-source:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;

    use http::header::HeaderValue;

    fn mapping_of(
        failure: StreamFailure,
        relayed: bool,
    ) -> (&'static str, u16, &'static str, bool) {
        let mapping = classify(failure, relayed).row.mapping();
        (
            mapping.variant,
            mapping.status,
            mapping.gts_type,
            mapping.retriable,
        )
    }

    #[test]
    fn sha1_matches_the_published_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // 55 bytes: the padding fits in the block the message occupies.
        assert_eq!(
            hex(&sha1(&[b'a'; 55])),
            "c1c8bbdc22796e28c0e15163d20899b65621d65a"
        );
        // 56 bytes: the length word needs a block of its own.
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // 64 bytes: exactly one whole block of message.
        assert_eq!(
            hex(&sha1(&[b'b'; 64])),
            "9d682ff9a7018603023176b8c12926c6a15510ee"
        );
        // 65 bytes: one whole block plus a partial one.
        assert_eq!(
            hex(&sha1(&[b'c'; 65])),
            "ead70b1c37df96502cc570570ec3481b5c61809f"
        );
    }

    fn hex(digest: &[u8]) -> String {
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn accept_matches_the_rfc_6455_vectors() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(
            websocket_accept("x3JJHMbDL1EzLkh9GBhXDw=="),
            "HSmrc0sMlYUkAGmm5OPpG2HaGWk="
        );
    }

    #[test]
    fn accept_verification_accepts_only_the_derived_value() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = websocket_accept(key);
        assert!(verify_websocket_accept(key, &accept));
        assert!(verify_websocket_accept(
            key,
            "  s3pPLMBiTxaQ9kYGzzhZRbK+xOo=  "
        ));
        assert!(!verify_websocket_accept(key, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo"));
        assert!(!verify_websocket_accept(key, ""));
        assert!(!verify_websocket_accept("another key entirely", &accept));
    }

    #[test]
    fn carve_out_exempts_exactly_three_families_for_an_upgrade() {
        for name in [
            "upgrade",
            "Upgrade",
            "connection",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
            "SEC-WEBSOCKET-PROTOCOL",
        ] {
            assert!(carve_out_keeps(name, true), "{name} must be kept");
        }
        for name in [
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "host",
            "authorization",
            "x-oagw-target-host",
            "content-length",
            "sec-websocketx",
            "upgradex",
        ] {
            assert!(!carve_out_keeps(name, true), "{name} must be stripped");
        }
    }

    #[test]
    fn carve_out_exempts_nothing_without_an_upgrade() {
        for name in [
            "upgrade",
            "connection",
            "sec-websocket-key",
            "keep-alive",
            "transfer-encoding",
            "authorization",
        ] {
            assert!(!carve_out_keeps(name, false), "{name} must be stripped");
        }
    }

    #[test]
    fn upgrade_detection_reads_the_request_side_only() {
        let mut headers = HeaderMap::new();
        headers.insert("upgrade", "websocket".parse().expect("header value"));
        assert!(is_websocket_upgrade(&headers));
        headers.insert("upgrade", "WebSocket".parse().expect("header value"));
        assert!(is_websocket_upgrade(&headers));
        headers.insert("upgrade", "foo, websocket".parse().expect("header value"));
        assert!(is_websocket_upgrade(&headers));
        headers.insert("upgrade", "h2c".parse().expect("header value"));
        assert!(!is_websocket_upgrade(&headers));
        headers.insert("upgrade", " WebSocket ".parse().expect("header value"));
        assert!(is_websocket_upgrade(&headers));
        headers.remove("upgrade");
        headers.insert("accept", "text/event-stream".parse().expect("header"));
        assert!(!is_websocket_upgrade(&headers));
        headers.insert(
            "upgrade",
            HeaderValue::from_bytes(&[0x80u8, 0xffu8]).expect("opaque header value"),
        );
        assert!(!is_websocket_upgrade(&headers));
    }

    /// The detection reads every member of the repeated `Upgrade` header, not
    /// only the last one inserted: a request carrying a non-websocket upgrade
    /// token in one member and `websocket` in another is a WebSocket upgrade,
    /// which is the multi-value form of `inst-ws-02`.
    #[test]
    fn upgrade_detection_reads_every_value_of_a_repeated_header() {
        let mut headers = HeaderMap::new();
        headers.append("upgrade", "h2c".parse().expect("header value"));
        assert!(
            !is_websocket_upgrade(&headers),
            "the first member names no websocket"
        );
        headers.append("upgrade", "websocket".parse().expect("header value"));

        assert!(
            is_websocket_upgrade(&headers),
            "the second member of the repeated header carries the upgrade token"
        );
        assert_eq!(
            headers.get_all("upgrade").iter().count(),
            2,
            "the fixture holds two members of the one header"
        );
    }

    #[test]
    fn scheme_eligibility_refuses_before_any_handshake() {
        assert!(upgrade_scheme_eligible(EndpointScheme::Wss).is_ok());
        assert!(upgrade_scheme_eligible(EndpointScheme::Http).is_ok());
        for scheme in [
            EndpointScheme::Https,
            EndpointScheme::Grpc,
            EndpointScheme::Wt,
        ] {
            let error = upgrade_scheme_eligible(scheme).expect_err("refused");
            let mapping = error.mapping();
            assert_eq!(mapping.status, 400);
            assert_eq!(mapping.variant, "RouteError");
            assert!(!mapping.retriable);
        }
    }

    #[test]
    fn classification_maps_each_failure_onto_its_row() {
        assert_eq!(
            mapping_of(StreamFailure::Idle, false),
            (
                "IdleTimeout",
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
                true
            )
        );
        assert_eq!(
            mapping_of(StreamFailure::Aborted, false),
            (
                "StreamAborted",
                502,
                "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
                false
            )
        );
        assert_eq!(
            mapping_of(StreamFailure::EstablishTimedOut, false),
            (
                "ConnectionTimeout",
                504,
                "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
                true
            )
        );
        assert_eq!(
            mapping_of(StreamFailure::EstablishUnreachable, false),
            (
                "LinkUnavailable",
                503,
                "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
                true
            )
        );
        for failure in [StreamFailure::EstablishAnswered, StreamFailure::Protocol] {
            assert_eq!(
                mapping_of(failure, false),
                (
                    "ProtocolError",
                    502,
                    "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
                    false
                )
            );
        }
    }

    #[test]
    fn classification_never_maps_an_abort_onto_downstream_error() {
        let outcome = classify(StreamFailure::Aborted, true);
        assert_eq!(outcome.row.mapping().variant, "StreamAborted");
        assert_ne!(outcome.row.mapping().variant, "DownstreamError");
        assert_eq!(outcome.row.mapping().status, 502);
    }

    #[test]
    fn classification_decides_the_delivery_surface_from_the_relayed_flag() {
        for failure in [
            StreamFailure::Idle,
            StreamFailure::Aborted,
            StreamFailure::EstablishTimedOut,
            StreamFailure::EstablishUnreachable,
            StreamFailure::EstablishAnswered,
            StreamFailure::Protocol,
        ] {
            assert!(classify(failure, false).deliverable, "{failure:?}");
            assert!(!classify(failure, true).deliverable, "{failure:?}");
        }
    }

    #[test]
    fn classification_is_total_over_the_failure_set() {
        for failure in [
            StreamFailure::Idle,
            StreamFailure::Aborted,
            StreamFailure::EstablishTimedOut,
            StreamFailure::EstablishUnreachable,
            StreamFailure::EstablishAnswered,
            StreamFailure::Protocol,
        ] {
            let outcome = classify(failure, false);
            assert!(!outcome.row.detail().is_empty(), "{failure:?}");
            assert!(!outcome.row.mapping().gts_type.is_empty(), "{failure:?}");
        }
    }

    #[test]
    fn every_streamed_row_is_an_existing_row_of_the_closed_table() {
        // No row is added for any outcome this feature produces: the variant a
        // classification maps onto is one the closed table already declares,
        // with the status, GTS type, title and retry guidance that row carries,
        // so no streaming-specific row, status or GTS type is invented.
        for failure in [
            StreamFailure::Idle,
            StreamFailure::Aborted,
            StreamFailure::EstablishTimedOut,
            StreamFailure::EstablishUnreachable,
            StreamFailure::EstablishAnswered,
            StreamFailure::Protocol,
        ] {
            let outcome = classify(failure, false);
            let mapping = outcome.row.mapping();
            let existing = OagwError::from_variant_name(mapping.variant, "occurrence")
                .unwrap_or_else(|| panic!("{} is not a row of the closed table", mapping.variant));
            let row = existing.mapping();
            assert_eq!(row.status, mapping.status, "{failure:?}");
            assert_eq!(row.gts_type, mapping.gts_type, "{failure:?}");
            assert_eq!(row.title, mapping.title, "{failure:?}");
            assert_eq!(row.retriable, mapping.retriable, "{failure:?}");
            // The detail the classification carries is this feature's static
            // text on the existing row, never a second row of its own.
            assert!(!outcome.row.detail().is_empty(), "{failure:?}");
        }
    }

    #[test]
    fn the_classified_details_are_static_and_carry_no_occurrence_content() {
        // The detail a streamed classification carries is the static text the
        // module fixes: no header value, no query string, no credential and no
        // frame content reaches the problem+json body the row is rendered as.
        const FORBIDDEN: [&str; 8] = [
            "cred://",
            "bearer ",
            "basic ",
            "authorization:",
            "cookie:",
            "api_key=",
            "?",
            "\n",
        ];
        for (failure, detail) in [
            (
                StreamFailure::Idle,
                "stream idle read reached the proxy timeout",
            ),
            (
                StreamFailure::Aborted,
                "upstream stream ended before the upstream ended it",
            ),
            (
                StreamFailure::EstablishTimedOut,
                "stream establishment reached the proxy timeout",
            ),
            (
                StreamFailure::EstablishUnreachable,
                "stream endpoint unreachable",
            ),
            (
                StreamFailure::EstablishAnswered,
                "stream handshake answer not a verifiable upgrade",
            ),
            (
                StreamFailure::Protocol,
                "stream handshake answer not a verifiable upgrade",
            ),
        ] {
            for relayed in [false, true] {
                let row = classify(failure, relayed).row;
                assert_eq!(row.detail(), detail, "{failure:?} relayed={relayed}");
                for marker in FORBIDDEN {
                    assert!(
                        !row.detail().contains(marker),
                        "{failure:?} carries {marker:?} in its detail"
                    );
                }
            }
        }
    }

    #[test]
    fn the_scheme_vocabulary_carries_no_ws_value_to_carry_an_upgrade() {
        // No `ws` scheme value is invented anywhere in this feature: the wire
        // names are the five the domain model declares, the wire form `ws`
        // parses to no scheme at all, and an exhaustive match over the enum
        // would not compile if a second upgrade scheme were added.
        for scheme in [
            EndpointScheme::Http,
            EndpointScheme::Https,
            EndpointScheme::Wss,
            EndpointScheme::Grpc,
            EndpointScheme::Wt,
        ] {
            let name = scheme.as_str();
            assert_ne!(name, "ws", "no ws scheme value exists");
            assert_eq!(EndpointScheme::parse(name), Some(scheme), "{name}");
        }
        assert!(EndpointScheme::parse("ws").is_none());
        // The schemes that can carry an upgrade are still exactly these two.
        assert!(upgrade_scheme_eligible(EndpointScheme::Wss).is_ok());
        assert!(upgrade_scheme_eligible(EndpointScheme::Http).is_ok());
    }

    #[test]
    fn session_opens_in_handed_over() {
        assert_eq!(StreamSession::new().state(), StreamSessionState::HandedOver);
    }

    #[test]
    fn session_walks_the_ten_declared_transitions() {
        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Detected)
            .expect("inst-ss-01");
        assert_eq!(session.state(), StreamSessionState::Detected);
        session
            .transition(StreamSessionState::Establishing)
            .expect("inst-ss-03");
        assert_eq!(session.state(), StreamSessionState::Establishing);
        session
            .transition(StreamSessionState::Streaming)
            .expect("inst-ss-05");
        assert_eq!(session.state(), StreamSessionState::Streaming);
        session
            .transition(StreamSessionState::Completed)
            .expect("inst-ss-07");
        assert_eq!(session.state(), StreamSessionState::Completed);
        session
            .transition(StreamSessionState::HandedOver)
            .expect("inst-ss-09");
        assert_eq!(session.state(), StreamSessionState::HandedOver);

        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Detected)
            .expect("inst-ss-01");
        session
            .transition(StreamSessionState::Failed)
            .expect("inst-ss-04");
        assert_eq!(session.state(), StreamSessionState::Failed);
        session
            .transition(StreamSessionState::HandedOver)
            .expect("inst-ss-10");
        assert_eq!(session.state(), StreamSessionState::HandedOver);

        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Completed)
            .expect("inst-ss-02");
        assert_eq!(session.state(), StreamSessionState::Completed);
    }

    /// The `Establishing -> Failed` hop, which no other walk of the declared
    /// transition set exercises: an establishment that fails reports the failure
    /// from `Establishing`, and the failed session still hands back over.
    #[test]
    fn session_walks_establishing_to_failed_and_back_to_handed_over() {
        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Detected)
            .expect("inst-ss-01");
        assert_eq!(session.state(), StreamSessionState::Detected);
        session
            .transition(StreamSessionState::Establishing)
            .expect("inst-ss-03");
        assert_eq!(session.state(), StreamSessionState::Establishing);
        session
            .transition(StreamSessionState::Failed)
            .expect("inst-ss-06");
        assert_eq!(session.state(), StreamSessionState::Failed);
        session
            .transition(StreamSessionState::HandedOver)
            .expect("inst-ss-10");
        assert_eq!(session.state(), StreamSessionState::HandedOver);
    }

    #[test]
    fn session_rejects_a_transition_not_declared() {
        let session = StreamSession::new();
        for to in [
            StreamSessionState::Establishing,
            StreamSessionState::Streaming,
            StreamSessionState::Failed,
        ] {
            session.transition(to).expect_err("not declared");
            assert_eq!(session.state(), StreamSessionState::HandedOver);
        }
        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Detected)
            .expect("inst-ss-01");
        for to in [
            StreamSessionState::Streaming,
            StreamSessionState::Completed,
            StreamSessionState::HandedOver,
        ] {
            session.transition(to).expect_err("not declared");
            assert_eq!(session.state(), StreamSessionState::Detected);
        }
        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Completed)
            .expect("inst-ss-02");
        for to in [
            StreamSessionState::Detected,
            StreamSessionState::Establishing,
            StreamSessionState::Streaming,
            StreamSessionState::Failed,
        ] {
            session
                .transition(to)
                .expect_err("terminal within one stream");
            assert_eq!(session.state(), StreamSessionState::Completed);
        }
    }

    #[test]
    fn session_is_cloned_per_stream_and_never_shared_across_requests() {
        let session = StreamSession::new();
        session
            .transition(StreamSessionState::Detected)
            .expect("inst-ss-01");
        let recorder = session.clone();
        recorder
            .transition(StreamSessionState::Establishing)
            .expect("inst-ss-03");
        assert_eq!(session.state(), StreamSessionState::Establishing);
        let fresh = StreamSession::default();
        assert_eq!(fresh.state(), StreamSessionState::HandedOver);
        assert_ne!(session.state(), StreamSessionState::HandedOver);
    }

    #[test]
    fn session_states_have_stable_names() {
        assert_eq!(StreamSessionState::HandedOver.as_str(), "HandedOver");
        assert_eq!(StreamSessionState::Detected.as_str(), "Detected");
        assert_eq!(StreamSessionState::Establishing.as_str(), "Establishing");
        assert_eq!(StreamSessionState::Streaming.as_str(), "Streaming");
        assert_eq!(StreamSessionState::Completed.as_str(), "Completed");
        assert_eq!(StreamSessionState::Failed.as_str(), "Failed");
    }
}
