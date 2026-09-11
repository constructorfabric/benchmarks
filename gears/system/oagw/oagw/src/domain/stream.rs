//! The streaming entities and the domain routines of the `oagw` gear.
//!
//! Realizes `cpt-cf-oagw-dod-stream-entities`, `cpt-cf-oagw-dod-stream-lifecycle`,
//! `cpt-cf-oagw-dod-stream-upgrade`, `cpt-cf-oagw-dod-stream-timeouts`, and
//! `cpt-cf-oagw-dod-stream-errors`: the two entities DECOMPOSITION §2.8 assigns
//! this entry, the one state machine it owns, and the parts of the §3 routines
//! that read no socket — [`upgrade_detection`] and [`select_mode`] of
//! `cpt-cf-oagw-algo-stream-mode-select`, and the suspension and the 101
//! judgement of `cpt-cf-oagw-algo-upgrade-handshake`. The third routine,
//! `cpt-cf-oagw-algo-stream-pump`, moves bytes between two live connection
//! halves and lives in the data-plane layer beside
//! `cpt-cf-oagw-algo-outbound-forward`, which opened the upstream half; the
//! [`StreamHalf`] values a session carries are the description of those halves
//! that layer updates, and never a socket, a stream, or a body type.
//!
//! The lifecycle state a session carries is a state of
//! `cpt-cf-oagw-state-stream-lifecycle` and not a third type beside the two
//! entities, which is why [`StreamLifecycle`] is the machine and
//! [`StreamSession`] only holds one of its states.

use std::time::Duration;

use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::proxy::ProxyContext;

// @cpt-dod:cpt-cf-oagw-dod-stream-entities:p1

/// The idle deadline of every streaming exchange, in seconds.
///
/// A build-time constant of this feature with no configuration surface and no
/// sourced value (`cpt-cf-oagw-dod-stream-timeouts`, §1.5): no key of
/// `OagwConfig` carries it, no upstream or route configuration reaches it, and
/// the session reads it from this constant and from nothing else.
pub const IDLE_TIMEOUT_SECS: u64 = 60;

/// The idle deadline of every streaming exchange.
///
/// It measures the absence of traffic in either direction and never the
/// duration of the exchange, because the pump resets it on every byte that
/// moves.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(IDLE_TIMEOUT_SECS);

/// The request headers a WebSocket handshake is judged by.
///
/// These four reach the upstream on a detected upgrade request regardless of
/// the resolved `headers.request.passthrough` mode, including at that mode's
/// shipped default of `none` (§1.5). The first two are the handshake's
/// required fields and the last two are forwarded when the caller offered
/// them; no other inbound header is admitted by the suspension.
pub const HANDSHAKE_HEADERS: [&str; 4] = [
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "sec-websocket-protocol",
];

/// Which of the two connection halves of a session a [`StreamHalf`] describes.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HalfSide {
    /// The caller's half, held by the platform's inbound handler.
    Caller,
    /// The upstream's half, opened by `cpt-cf-oagw-algo-outbound-forward`.
    Upstream,
}

/// One of the two connection halves a [`StreamSession`] describes.
///
/// A half is carried as the fact that it is open and not as the connection
/// itself, which is what keeps the entity free of transport types: the data
/// plane holds the connections and updates this member as it tears each one
/// down.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHalf {
    /// Which half of the exchange this is.
    pub side: HalfSide,
    /// Whether the half still carries bytes.
    pub open: bool,
}

impl StreamHalf {
    /// The caller's half of a session that has just been opened.
    #[must_use]
    pub fn caller() -> Self {
        Self {
            side: HalfSide::Caller,
            open: true,
        }
    }

    /// The upstream's half of a session that has just been opened.
    #[must_use]
    pub fn upstream() -> Self {
        Self {
            side: HalfSide::Upstream,
            open: true,
        }
    }
}

/// How a response body moves: as a bidirectional tunnel, or as a one-way
/// sequence of chunks the upstream emits.
///
/// The mode has exactly two values and no third one that buffers a complete
/// response body, because `cpt-cf-oagw-principle-no-cache` forbids holding a
/// response (§1.5).
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferMode {
    /// The body of a taken-up handshake, moved in both directions between the
    /// two halves and framed by nothing.
    Tunnel,
    /// Every other body, moved upstream to caller and flushed as it arrives.
    Incremental,
}

/// How a streaming exchange ended, recorded on the session that ended.
///
/// The two teardown directions are clean closes and are not error answers;
/// the other two are the only outcomes this feature answers through
/// [`answer_of`].
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// The caller disconnected: the gateway closed the upstream half.
    ClientDisconnected,
    /// The upstream closed its half: the gateway closed the caller's half.
    UpstreamClosed,
    /// No byte moved in either direction for the idle deadline.
    Stalled,
    /// A half failed while bytes were still expected, whichever side.
    Aborted,
}

/// Why an exchange ended without a transfer, recorded on the session the
/// handshake opened in `Opening`.
///
/// A session that never opened has no bytes in flight and takes the refusal
/// transition instead of the teardown ones, which is the only reason this
/// value exists: the answer the caller receives for such an exchange comes
/// from the routine that reports the failure, and never from [`answer_of`].
// @cpt-dod:cpt-cf-oagw-dod-stream-lifecycle:p1
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamLifecycle {
    /// The outbound handshake request has been sent and no answer has arrived.
    Opening,
    /// The upstream took the handshake up, or the response headers of a body
    /// transfer have arrived.
    Open,
    /// One side signalled the end and the other half is being torn down.
    Closing,
    /// Both halves are torn down and the outcome is recorded.
    Closed,
}

/// Why a lifecycle transition was refused.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{transition} is not a transition of the {from:?} state")]
pub struct StreamTransition {
    /// The state the refused transition was taken from.
    pub from: StreamLifecycle,
    /// The name of the refused transition.
    pub transition: &'static str,
}

impl StreamLifecycle {
    /// The `Opening` to `Open` transition, on the answer that takes the
    /// handshake up.
    ///
    /// # Errors
    /// Returns [`StreamTransition`] for every state but `Opening`: a session
    /// that is not in `Opening` has no handshake in flight to take up, and
    /// `Closed` is terminal.
    pub fn opened(self) -> Result<Self, StreamTransition> {
        // @cpt-begin:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-open
        match self {
            Self::Opening => Ok(Self::Open),
            other => Err(StreamTransition {
                from: other,
                transition: "opened",
            }),
        }
        // @cpt-end:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-open
    }

    /// The `Opening` to `Closed` transition, on a refusal or a failure that
    /// happened before any byte moved.
    ///
    /// # Errors
    /// Returns [`StreamTransition`] for every state but `Opening`: a session
    /// that opened takes the teardown transitions instead, and `Closed` is
    /// terminal.
    pub fn refused(self) -> Result<Self, StreamTransition> {
        // @cpt-begin:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-refused
        match self {
            Self::Opening => Ok(Self::Closed),
            other => Err(StreamTransition {
                from: other,
                transition: "refused",
            }),
        }
        // @cpt-end:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-refused
    }

    /// The `Open` to `Closing` transition, on one side signalling the end.
    ///
    /// # Errors
    /// Returns [`StreamTransition`] for every state but `Open`: a session that
    /// never opened has no bytes in flight to drain and takes the refusal
    /// transition instead, and a session that is closing is already draining.
    pub fn closing(self) -> Result<Self, StreamTransition> {
        // @cpt-begin:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-closing
        match self {
            Self::Open => Ok(Self::Closing),
            other => Err(StreamTransition {
                from: other,
                transition: "closing",
            }),
        }
        // @cpt-end:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-closing
    }

    /// The `Closing` to `Closed` transition, on the other half being torn
    /// down and the outcome recorded.
    ///
    /// # Errors
    /// Returns [`StreamTransition`] for every state but `Closing`, because no
    /// byte is in flight in either direction when a session leaves `Closing`
    /// and no other state owes a drain.
    pub fn closed(self) -> Result<Self, StreamTransition> {
        // @cpt-begin:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-closed
        match self {
            Self::Closing => Ok(Self::Closed),
            other => Err(StreamTransition {
                from: other,
                transition: "closed",
            }),
        }
        // @cpt-end:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-closed
    }

    /// The `Open` to `Closed` transition, on a mid-flight failure that aborts
    /// the transfer on either side.
    ///
    /// This is the only transition that bypasses `Closing`, because a failed
    /// half has nothing to drain and both halves are torn down together.
    ///
    /// # Errors
    /// Returns [`StreamTransition`] for every state but `Open`: a session
    /// that never opened was never transferring, and `Closed` is terminal.
    pub fn aborted(self) -> Result<Self, StreamTransition> {
        // @cpt-begin:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-abort
        match self {
            Self::Open => Ok(Self::Closed),
            other => Err(StreamTransition {
                from: other,
                transition: "aborted",
            }),
        }
        // @cpt-end:cpt-cf-oagw-state-stream-lifecycle:p1:inst-state-abort
    }
}

/// One streaming exchange: its two connection halves, the transfer mode
/// selected for it, the response `Content-Type` recorded for it, the lifecycle
/// state it is in, the deadlines in force over it, and the outcome recorded
/// when it ended.
///
/// The entity lives only as long as the two connections it describes and is
/// dropped with them: no state of [`StreamLifecycle`] is persisted, no table
/// of `cpt-cf-oagw-db-schema` is written by any routine of the feature, and a
/// restart changes no answer the feature gives.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct StreamSession {
    /// The tenant the proxied request addressed.
    pub tenant_id: Uuid,
    /// The upstream the exchange is carried to.
    pub upstream_id: Uuid,
    /// The transfer mode the response's headers selected.
    pub mode: TransferMode,
    /// The response `Content-Type` recorded for the session.
    pub content_type: Option<String>,
    /// The state of `cpt-cf-oagw-state-stream-lifecycle` the session is in.
    pub lifecycle: StreamLifecycle,
    /// The idle deadline in force over the transfer.
    pub idle_timeout: Duration,
    /// The header-arrival deadline `cpt-cf-oagw-algo-outbound-forward` applies,
    /// in force only while the session is in `Opening` and only for a session
    /// whose send has begun; the body's only deadline is [`IDLE_TIMEOUT`].
    pub header_timeout: Option<Duration>,
    /// The caller's half.
    pub caller: StreamHalf,
    /// The upstream's half.
    pub upstream: StreamHalf,
    /// The bytes that have been read from one half and written and flushed to
    /// the other, which is what the idle timer measures.
    pub moved: u64,
    /// The outcome recorded when the exchange ended.
    pub outcome: Option<StreamOutcome>,
}

impl StreamSession {
    /// Opens the session of a body transfer, in the `Open` state.
    ///
    /// An `incremental` session is never in `Opening`, because the feature is
    /// first reached at the point the upstream's response headers have arrived
    /// and there is no in-flight window for it to describe (§4).
    #[must_use]
    pub fn open_for_incremental(
        tenant_id: Uuid,
        upstream_id: Uuid,
        content_type: Option<String>,
    ) -> Self {
        Self {
            tenant_id,
            upstream_id,
            mode: TransferMode::Incremental,
            content_type,
            lifecycle: StreamLifecycle::Open,
            idle_timeout: IDLE_TIMEOUT,
            header_timeout: None,
            caller: StreamHalf::caller(),
            upstream: StreamHalf::upstream(),
            moved: 0,
            outcome: None,
        }
    }

    /// Opens the session of a taken-up handshake, in the `Opening` state.
    ///
    /// The session exists for as long as the handshake is in flight and
    /// carries the `tunnel` mode from the start, because the mode is what the
    /// detection that preceded the send fixed.
    #[must_use]
    pub fn open_for_handshake(tenant_id: Uuid, upstream_id: Uuid) -> Self {
        Self {
            tenant_id,
            upstream_id,
            mode: TransferMode::Tunnel,
            content_type: None,
            lifecycle: StreamLifecycle::Opening,
            idle_timeout: IDLE_TIMEOUT,
            header_timeout: None,
            caller: StreamHalf::caller(),
            upstream: StreamHalf::upstream(),
            moved: 0,
            outcome: None,
        }
    }

    /// Closes a session whose handshake the upstream refused or whose send
    /// failed before any byte moved, so no half survives it.
    ///
    /// The exchange was terminated before data, which is why the outcome
    /// recorded is [`StreamOutcome::Aborted`]; the answer the caller receives
    /// for such an exchange is the one `cpt-cf-oagw-algo-outbound-forward`
    /// reports or the upstream's own answer, and never [`answer_of`].
    pub fn refuse(&mut self) {
        if let Ok(closed) = self.lifecycle.refused() {
            self.lifecycle = closed;
            self.caller.open = false;
            self.upstream.open = false;
            self.outcome = Some(StreamOutcome::Aborted);
        }
    }

    /// Ends the session because the caller disconnected, which closes the
    /// upstream half and takes the lifecycle through `Closing` to `Closed`.
    pub fn disconnect(&mut self) {
        self.teardown(StreamOutcome::ClientDisconnected);
    }

    /// Ends the session because the upstream closed its half, which closes the
    /// caller's half and takes the lifecycle through `Closing` to `Closed`.
    pub fn upstream_closed(&mut self) {
        self.teardown(StreamOutcome::UpstreamClosed);
    }

    /// Ends the session because no byte moved in either direction for the idle
    /// deadline, which tears both halves down.
    pub fn stalled(&mut self) {
        self.abort(StreamOutcome::Stalled);
    }

    /// Ends the session because a half failed while bytes were still expected,
    /// whichever side failed, which tears both halves down together.
    pub fn abort_transfer(&mut self) {
        self.abort(StreamOutcome::Aborted);
    }

    /// Counts bytes once they have been written and flushed to the other half,
    /// which is the event the idle timer measures.
    pub fn record_moved(&mut self, bytes: u64) {
        self.moved += bytes;
    }

    /// Whether the session is still in the `Open` state with no outcome
    /// recorded, which is the state a transfer that ended without an upstream
    /// end, a stall, or an abort was left in: the caller's half went away and
    /// the pump never learned why.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.lifecycle == StreamLifecycle::Open && self.outcome.is_none()
    }

    /// Takes the lifecycle through `Closing` to `Closed` for a clean end, and
    /// tears both halves down with it.
    fn teardown(&mut self, outcome: StreamOutcome) {
        if self.lifecycle.closing().is_ok() {
            self.lifecycle = StreamLifecycle::Closing;
        }
        if let Ok(closed) = self.lifecycle.closed() {
            self.lifecycle = closed;
            self.caller.open = false;
            self.upstream.open = false;
            self.outcome = Some(outcome);
        }
    }

    /// Moves the lifecycle straight to `Closed`, bypassing `Closing`, for an
    /// exchange that was terminated with bytes still expected.
    fn abort(&mut self, outcome: StreamOutcome) {
        if let Ok(closed) = self.lifecycle.aborted() {
            self.lifecycle = closed;
            self.caller.open = false;
            self.upstream.open = false;
            self.outcome = Some(outcome);
        }
    }
}

/// The upgrade request the three-part detection identified.
///
/// The detection's only content is the judgement itself: the values that
/// satisfied the three parts stay in the request the proxy path holds, which
/// is where the suspension reads them from. The type is `Copy` for the same
/// reason, because the detection is held on the request from the
/// header-transformation step to the send and consumed once more after the
/// response headers arrive.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeDetection;

/// How the handshake ended, judged from the upstream's answer.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpgradeAnswer {
    /// The answer has not arrived.
    NotJudged,
    /// The upstream answered 101 and the handshake was taken up.
    Taken,
    /// The upstream answered anything else, which passes through unchanged.
    NotTaken,
    /// The send failed before the upstream answered at all.
    FailedBeforeData,
}

/// One upgrade exchange: the outbound handshake request's suspended headers
/// and the upstream's answer.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeHandshake {
    /// The header pairs the outbound handshake request carries: the two
    /// hop-by-hop headers whose strip is suspended, and the handshake's own
    /// request headers, in the values the caller sent.
    pub suspended: Vec<(String, String)>,
    /// The upstream's answer, or the fact that the handshake failed before
    /// data.
    pub answer: UpgradeAnswer,
}

impl UpgradeHandshake {
    /// Builds the handshake of a detected upgrade request from the headers the
    /// caller sent.
    ///
    /// The suspension this records is the DESIGN §3.2 Headers Transformation
    /// upgrade exception, which `cpt-cf-oagw-algo-header-transform` explicitly
    /// does not apply: it is scoped to two of the eight hop-by-hop headers and
    /// to the request direction, and it changes nothing else about the request
    /// the proxy path would have sent.
    #[must_use]
    pub fn build(_detection: UpgradeDetection, context: &ProxyContext) -> Self {
        let mut suspended: Vec<(String, String)> = Vec::new();
        // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-suspend
        // The two headers whose strip the handshake suspends, in the values
        // the caller sent, so the handshake reaches the upstream intact. The
        // other six hop-by-hop headers of DESIGN §3.2's table stay stripped
        // exactly as the unconditional rule strips them, which is why they are
        // not recorded here: `cpt-cf-oagw-algo-header-transform` removes them
        // under the suspension, and `cpt-cf-oagw-algo-upgrade-handshake`'s
        // `inst-uh-six` step is that strip running unchanged.
        for name in ["Upgrade", "Connection"] {
            for value in context.header_values(name) {
                suspended.push((String::from(name), String::from(value)));
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-suspend
        // The handshake's own request headers are recorded with the suspended
        // two, because they are the fields a handshake is judged by and the
        // shipped `passthrough` default would otherwise forward none of them.
        // Admitting them over that mode is `cpt-cf-oagw-algo-header-transform`'s
        // act, which is where the suspension is applied.
        for (name, value) in &context.headers {
            if HANDSHAKE_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                suspended.push((String::from(name), String::from(value)));
            }
        }
        Self {
            suspended,
            answer: UpgradeAnswer::NotJudged,
        }
    }

    /// Records the handshake as failed before data, which the caller answers
    /// through the variants `cpt-cf-oagw-algo-outbound-forward` names.
    pub fn failed_before_data(&mut self) {
        self.answer = UpgradeAnswer::FailedBeforeData;
    }

    /// Judges the handshake by the upstream's answer status.
    ///
    /// Only a 101 completes the handshake; any other answer is judged not
    /// taken up, which passes through unchanged under the error-source
    /// classification and leaves the connection a plain request/response
    /// exchange, with no variant of the catalogue substituted for the answer
    /// the upstream itself produced.
    pub fn judge(&mut self, status: u16) {
        // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-if
        if status == 101 {
            // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101
            self.answer = UpgradeAnswer::Taken;
            // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101
            return;
        }
        // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-if
        // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-else
        // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-not-101
        self.answer = UpgradeAnswer::NotTaken;
        // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-not-101
        // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-else
    }
}

/// What the mode-selection routine attaches to the mode it selected: the 101
/// answer of a tunnel, or the response `Content-Type` of a body transfer.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCarry {
    /// The upstream's answer to a taken-up handshake.
    Answer(u16),
    /// The response `Content-Type` recorded for the session.
    ContentType(String),
}

/// Reads the three parts of the upgrade detection from the request.
///
/// The method must be `GET`, the `Upgrade` header must name `websocket`, and
/// the `Connection` header must name the `upgrade` token. The last two are
/// compared case-insensitively, and `Connection` is matched over a
/// comma-separated token list; an `Upgrade` value carrying whitespace or a
/// list of protocols is matched against the one literal and against nothing
/// else, because the routine neither normalizes nor repairs a header value.
///
/// The routine runs before `cpt-cf-oagw-algo-header-transform` builds the
/// outbound header map and before any strip runs, which is why it answers only
/// the detection question: the response does not exist yet.
// @cpt-dod:cpt-cf-oagw-dod-stream-upgrade:p1
#[must_use]
pub fn upgrade_detection(
    method: &str,
    upgrade: Option<&str>,
    connection: Option<&str>,
) -> Option<UpgradeDetection> {
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-request
    // The three parts are read from the request as the proxy path holds them,
    // before `cpt-cf-oagw-algo-header-transform` builds the outbound header
    // map and before any strip runs. A method outside the shipped route
    // schema's five literals cannot reach here, and the values are matched as
    // the caller sent them and never normalized.
    if method != "GET" {
        return None;
    }
    let (upgrade, connection) = (upgrade?, connection?);
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-request
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-upgrade-if
    let parts_hold = upgrade.trim().eq_ignore_ascii_case("websocket")
        && connection
            .split(',')
            .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if parts_hold {
        // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-upgrade-return
        // The detection is returned so `cpt-cf-oagw-algo-upgrade-handshake`
        // builds the handshake and applies the suspension, and it is held on
        // the request for the second half to consume after the send.
        return Some(UpgradeDetection);
        // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-upgrade-return
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-upgrade-if
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-upgrade-else
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-not-upgrade
    // No detection: the strip runs over all eight hop-by-hop headers and the
    // exchange proceeds as a plain request/response transfer.
    None
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-not-upgrade
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-upgrade-else
}

/// Selects the transfer mode of a response body from the request and the
/// response headers.
///
/// The routine runs in two halves at the two invocation points §1.5 records.
/// The first half is [`upgrade_detection`], which runs before the outbound
/// header map is built; this is the second half, which runs after the response
/// headers arrive and answers only the mode question, because the request has
/// already been sent. The mode is `tunnel` when the request was an upgrade
/// request and the answer is 101, and `incremental` for every other body,
/// whether or not its content type is `text/event-stream`; neither value
/// buffers a complete response body.
// @cpt-dod:cpt-cf-oagw-dod-stream-timeouts:p1
#[must_use]
pub fn select_mode(
    detection: Option<UpgradeDetection>,
    status: u16,
    content_type: Option<&str>,
) -> (TransferMode, Option<SessionCarry>) {
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-response
    // The detection the first half recorded is the one this half consumes; the
    // status and the content type are read from the response headers
    // `cpt-cf-oagw-algo-outbound-forward` received.
    let upgraded = detection.is_some();
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-tunnel-if
    if upgraded && status == 101 {
        // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-tunnel
        let mode = TransferMode::Tunnel;
        let carry = SessionCarry::Answer(status);
        // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-tunnel
        // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-tunnel-if
        return (mode, Some(carry));
    }
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-response
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-tunnel-else
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-incremental
    // The mode is the same value for `text/event-stream` and for every other
    // body, and neither buffers a complete response body.
    let mode = TransferMode::Incremental;
    let carry = content_type.map(|value| SessionCarry::ContentType(String::from(value)));
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-incremental
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-tunnel-else
    // @cpt-begin:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-return
    (mode, carry)
    // @cpt-end:cpt-cf-oagw-algo-stream-mode-select:p1:inst-sms-return
}

/// The error answer an outcome of the pump is answered with.
///
/// A stalled stream is answered 504 with `IdleTimeout`
/// (`gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`) and a mid-flight
/// termination 502 with `StreamAborted`
/// (`gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`), both carrying
/// `X-OAGW-Error-Source: gateway` through
/// `cpt-cf-oagw-algo-error-mapping`. Neither answer sets
/// `retry_after_seconds`, so neither carries `Retry-After`, including
/// `IdleTimeout`, which the catalogue marks retriable and which is answered
/// without the header because the gateway has no interval to state for a
/// stream that died (§1.5).
///
/// The two teardown directions are clean closes and are answered with the
/// completed transfer rather than with an error, so they name no answer.
// @cpt-dod:cpt-cf-oagw-dod-stream-errors:p1
#[must_use]
pub fn answer_of(outcome: StreamOutcome) -> Option<DomainError> {
    match outcome {
        StreamOutcome::Stalled => Some(DomainError::gateway(
            ErrorKind::IdleTimeout,
            "the stream moved no byte in either direction for the idle deadline",
        )),
        StreamOutcome::Aborted => Some(DomainError::gateway(
            ErrorKind::StreamAborted,
            "the stream was terminated mid-flight while bytes were still expected",
        )),
        StreamOutcome::ClientDisconnected | StreamOutcome::UpstreamClosed => None,
    }
}
