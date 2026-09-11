//! Stream lifecycle entities and stream error classification (entry 2.6).
//!
//! The module holds the two lifecycles the streaming feature runs — the
//! exchanged handed over at the entry-2.4 handoff
//! (`cpt-cf-oagw-state-stream-lifecycle`) and the WebSocket session an upgrade
//! establishes inside it (`cpt-cf-oagw-state-ws-session`) — the outcome both of
//! them record for entry 2.7, and the classifier
//! (`cpt-cf-oagw-algo-stream-error-classify`) that maps a stream failure onto
//! exactly one row of the canonical error table.
//!
//! The layer is a pure domain layer: no HTTP type, no transport type and no
//! runtime handle is named here. The state machines advance on the events the
//! forwarding and relay stages raise (`cpt-cf-oagw-algo-stream-lifecycle`), and
//! every terminal transition records the close reason, the failing direction,
//! the byte counts and the error type on a [`StreamOutcome`] that carries no
//! header value, no request body byte and no query string.
//!
//! A stream record is held in memory for the life of one exchange only
//! (DECOMPOSITION assumption 3): a process restart ends every stream the
//! process was relaying and leaves no lifecycle to recover
//! (`inst-ss-lif-09`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;

use crate::domain::error::DomainError;

/// The side of an exchange that closed it or failed it.
pub const SIDE_CLIENT: &str = "client";

/// The upstream side of an exchange.
pub const SIDE_UPSTREAM: &str = "upstream";

/// The side of an exchange that closed it, when no side did: an exchange the
/// gateway itself answered.
pub const SIDE_NONE: &str = "none";

/// What kind of streamed exchange the handoff declared
/// (`cpt-cf-oagw-algo-stream-lifecycle` input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    /// A response whose `Content-Type` is `text/event-stream`.
    SseResponse,
    /// A request whose `Accept` header names `text/event-stream`.
    SseRequest,
    /// A negotiated WebSocket upgrade.
    WebSocket,
}

impl StreamKind {
    /// The wire token the observability layer labels the stream kind with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SseResponse => "sse_response",
            Self::SseRequest => "sse_request",
            Self::WebSocket => "websocket",
        }
    }
}

/// A state of the streamed exchange (`cpt-cf-oagw-state-stream-lifecycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// The exchange was handed over and nothing was relayed yet.
    HandedOff,
    /// The response head was relayed to the client.
    Open,
    /// The first body chunk was flushed to the client.
    Relaying,
    /// The upstream ended the stream. Terminal.
    Closed,
    /// A side dropped or the exchange failed. Terminal.
    Aborted,
    /// The idle window elapsed with no byte in either direction. Terminal.
    TimedOut,
}

impl StreamState {
    /// The wire token the observability layer labels the state with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HandedOff => "handed_off",
            Self::Open => "open",
            Self::Relaying => "relaying",
            Self::Closed => "closed",
            Self::Aborted => "aborted",
            Self::TimedOut => "timed_out",
        }
    }

    /// Whether the state ends the exchange: no transition leaves a terminal
    /// state and no stream is re-entered after it ends (`inst-ss-lif-08`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Closed | Self::Aborted | Self::TimedOut)
    }
}

/// A state of the WebSocket session (`cpt-cf-oagw-state-ws-session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsState {
    /// The upgrade request was handed over and no upstream call was made yet.
    Upgrading,
    /// The upstream answered `101` and the head was relayed verbatim.
    Established,
    /// The first frame was read on either side and forwarded.
    Relaying,
    /// The upgrade was refused: a non-`101` answer, the plaintext gate or a
    /// malformed request. Terminal.
    Rejected,
    /// A side closed the session. Terminal.
    Closed,
    /// The attempt failed or a relay side failed. Terminal.
    Aborted,
    /// The idle window elapsed with no frame in either direction. Terminal.
    TimedOut,
}

impl WsState {
    /// The wire token the observability layer labels the session state with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upgrading => "upgrading",
            Self::Established => "established",
            Self::Relaying => "relaying",
            Self::Rejected => "rejected",
            Self::Closed => "closed",
            Self::Aborted => "aborted",
            Self::TimedOut => "timed_out",
        }
    }

    /// Whether the state ends the session: no transition leaves a terminal
    /// state and no session is upgraded twice (`inst-ss-lif-08`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Rejected | Self::Closed | Self::Aborted | Self::TimedOut
        )
    }
}

/// Why an exchange or a session ended.
///
/// The reason is a routing fact entry 2.7 reports; it is never a header value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// The upstream ended the exchange.
    UpstreamClosed,
    /// The client went away.
    ClientDisconnected,
    /// A side failed or the exchange was torn down.
    Aborted,
    /// The idle window elapsed.
    IdleTimeout,
    /// The upgrade was refused and an answer was produced for the client.
    Rejected,
}

impl CloseReason {
    /// The wire token the observability layer labels the close reason with.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpstreamClosed => "upstream_closed",
            Self::ClientDisconnected => "client_disconnected",
            Self::Aborted => "aborted",
            Self::IdleTimeout => "idle_timeout",
            Self::Rejected => "rejected",
        }
    }
}

/// The outcome a terminal transition records (`inst-ss-lif-03`).
///
/// Every member is a measurement or a routing fact: no credential material, no
/// header value, no request body byte and no query string is in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamOutcome {
    /// Why the exchange ended.
    pub reason: CloseReason,
    /// The side that closed the exchange: [`SIDE_CLIENT`], [`SIDE_UPSTREAM`] or
    /// [`SIDE_NONE`].
    pub closing_side: &'static str,
    /// Bytes the upstream sent and the gateway relayed to the client.
    pub bytes_downstream: u64,
    /// Bytes the client sent and the gateway relayed to the upstream.
    pub bytes_upstream: u64,
    /// The GTS `type` identifier of the failure the exchange ended in
    /// (`inst-ss-cls-10`).
    pub error_type: Option<&'static str>,
    /// The idle window that was armed for the exchange, in seconds
    /// (`inst-ss-tmo-07`).
    pub idle_window_secs: Option<u64>,
}

impl StreamOutcome {
    /// The outcome a terminal transition records.
    #[must_use]
    pub fn terminal(
        reason: CloseReason,
        closing_side: &'static str,
        bytes_downstream: u64,
        bytes_upstream: u64,
        error_type: Option<&'static str>,
        idle_window_secs: Option<u64>,
    ) -> Self {
        Self {
            reason,
            closing_side,
            bytes_downstream,
            bytes_upstream,
            error_type,
            idle_window_secs,
        }
    }
}

/// The events the forwarding and relay stages raise
/// (`inst-ss-lif-02`, `cpt-cf-oagw-state-stream-lifecycle`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEvent {
    /// The upstream response head was relayed to the client
    /// (`inst-ss-stl-01`).
    HeadRelayed,
    /// The first body chunk was received and flushed to the client
    /// (`inst-ss-stl-04`).
    ChunkFlushed,
    /// The upstream ended the stream (`inst-ss-stl-05`, `inst-ss-stl-08`).
    UpstreamEnded,
    /// A side dropped or the exchange failed (`inst-ss-stl-02`, `inst-ss-stl-06`,
    /// `inst-ss-stl-09`).
    Failed {
        /// The side that failed the exchange.
        side: &'static str,
        /// Why the side is recorded as the closing one.
        reason: CloseReason,
    },
    /// The idle window elapsed with no byte in either direction
    /// (`inst-ss-stl-03`, `inst-ss-stl-07`, `inst-ss-stl-10`).
    IdleElapsed,
}

/// The lifecycle of one streamed exchange
/// (`cpt-cf-oagw-state-stream-lifecycle`).
///
/// The machine starts in `handed_off`, advances on exactly the ten transitions
/// the artifact declares and never leaves a terminal state: `closed`, `aborted`
/// and `timed_out` end the exchange, no state is skipped and the same exchange
/// is never relayed twice.
#[derive(Debug)]
pub struct StreamLifecycle {
    state: StreamState,
    bytes_downstream: u64,
    bytes_upstream: u64,
    idle_window_secs: Option<u64>,
    error_type: Option<&'static str>,
    outcome: Option<StreamOutcome>,
}

impl StreamLifecycle {
    /// A lifecycle for an exchange handed over by the pipeline
    /// (`inst-ss-lif-01`).
    #[must_use]
    pub fn new(idle_window_secs: Option<u64>) -> Self {
        Self {
            state: StreamState::HandedOff,
            bytes_downstream: 0,
            bytes_upstream: 0,
            idle_window_secs,
            error_type: None,
            outcome: None,
        }
    }

    /// The state the machine is in.
    #[must_use]
    pub const fn state(&self) -> StreamState {
        self.state
    }

    /// The outcome the terminal transition recorded, when the exchange ended.
    #[must_use]
    pub const fn outcome(&self) -> Option<&StreamOutcome> {
        self.outcome.as_ref()
    }

    /// Consume the machine into the outcome it recorded.
    #[must_use]
    pub fn into_outcome(self) -> Option<StreamOutcome> {
        self.outcome
    }

    /// Add the byte count of a chunk relayed downstream
    /// (`inst-ss-sse-11`, `inst-ss-lif-03`).
    pub fn count_downstream(&mut self, bytes: u64) {
        self.bytes_downstream += bytes;
    }

    /// Add the byte count of a chunk or a frame received upstream
    /// (`inst-ss-rlb-04`, `inst-ss-lif-03`).
    pub fn count_upstream(&mut self, bytes: u64) {
        self.bytes_upstream += bytes;
    }

    /// Record the GTS `type` identifier of the failure the exchange ends in
    /// (`inst-ss-cls-10`).
    ///
    /// The terminal transition copies it into the outcome, so a failure is
    /// recorded before the event that ends the exchange.
    pub fn record_error(&mut self, error: &DomainError) {
        self.error_type = Some(error.gts_id());
    }

    /// Advance the machine on `event`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `502` of a transition
    /// `cpt-cf-oagw-state-stream-lifecycle` does not declare, which is a
    /// programming fault of the relay rather than a caller fault: no state is
    /// skipped, none is re-entered and a terminal state is never left.
    pub fn transition(&mut self, event: StreamEvent) -> Result<(), DomainError> {
        if !Self::allows(self.state, event) {
            return Err(DomainError::ProtocolError {
                detail: format!(
                    "the stream lifecycle cannot move from {} on this event",
                    self.state.as_str()
                ),
            });
        }
        self.state = Self::target(event);
        if self.state.is_terminal() {
            self.outcome = Some(self.outcome_of(event));
        }
        Ok(())
    }

    /// Whether the machine allows the transition `state` takes on `event`.
    #[must_use]
    const fn allows(state: StreamState, event: StreamEvent) -> bool {
        use StreamEvent::{ChunkFlushed, Failed, HeadRelayed, IdleElapsed, UpstreamEnded};
        matches!(
            (state, event),
            (StreamState::HandedOff, HeadRelayed)
                | (StreamState::HandedOff, Failed { .. })
                | (StreamState::HandedOff, IdleElapsed)
                | (StreamState::Open, ChunkFlushed)
                | (StreamState::Open, UpstreamEnded)
                | (StreamState::Open, Failed { .. })
                | (StreamState::Open, IdleElapsed)
                | (StreamState::Relaying, UpstreamEnded)
                | (StreamState::Relaying, Failed { .. })
                | (StreamState::Relaying, IdleElapsed)
        )
    }

    /// The state the machine reaches on `event`.
    const fn target(event: StreamEvent) -> StreamState {
        match event {
            StreamEvent::HeadRelayed => StreamState::Open,
            StreamEvent::ChunkFlushed => StreamState::Relaying,
            StreamEvent::UpstreamEnded => StreamState::Closed,
            StreamEvent::Failed { .. } => StreamState::Aborted,
            StreamEvent::IdleElapsed => StreamState::TimedOut,
        }
    }

    /// The outcome the terminal transition records.
    fn outcome_of(&self, event: StreamEvent) -> StreamOutcome {
        let (reason, closing_side) = match event {
            StreamEvent::UpstreamEnded => (CloseReason::UpstreamClosed, SIDE_UPSTREAM),
            StreamEvent::Failed { side, reason } => (reason, side),
            StreamEvent::IdleElapsed => (CloseReason::IdleTimeout, SIDE_NONE),
            // The head was relayed or the first chunk was flushed: the exchange
            // did not end, so no outcome is recorded for them.
            StreamEvent::HeadRelayed | StreamEvent::ChunkFlushed => {
                (CloseReason::UpstreamClosed, SIDE_NONE)
            }
        };
        StreamOutcome::terminal(
            reason,
            closing_side,
            self.bytes_downstream,
            self.bytes_upstream,
            self.error_type,
            self.idle_window_secs,
        )
    }
}

/// The events the upgrade and relay stages raise
/// (`cpt-cf-oagw-state-ws-session`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsEvent {
    /// The upstream answered `101` and the head was relayed verbatim
    /// (`inst-ss-stw-01`).
    Established,
    /// The upstream refused the upgrade, the plaintext gate refused the scheme
    /// or the request was malformed (`inst-ss-stw-02`).
    Refused,
    /// The upgrade attempt itself failed (`inst-ss-stw-03`).
    AttemptFailed,
    /// The first frame was read on either side and forwarded
    /// (`inst-ss-stw-04`).
    FrameRelayed,
    /// A side closed the session (`inst-ss-stw-05`, `inst-ss-stw-07`).
    Closed {
        /// The side that closed first.
        side: &'static str,
    },
    /// A read or a write failed, or a side disconnected abruptly
    /// (`inst-ss-stw-06`, `inst-ss-stw-08`).
    Failed {
        /// The side that failed the session.
        side: &'static str,
        /// Why the side is recorded as the closing one.
        reason: CloseReason,
    },
    /// No frame was relayed for the idle window (`inst-ss-stw-09`,
    /// `inst-ss-stw-10`).
    IdleElapsed,
}

/// The session of one WebSocket upgrade
/// (`cpt-cf-oagw-state-ws-session`).
///
/// The machine starts in `upgrading`, advances on exactly the ten transitions
/// the artifact declares and never leaves a terminal state: `rejected`,
/// `closed`, `aborted` and `timed_out` end the session, no state is skipped and
/// a session is never upgraded twice.
#[derive(Debug)]
pub struct WsSession {
    state: WsState,
    bytes_downstream: u64,
    bytes_upstream: u64,
    idle_window_secs: Option<u64>,
    error_type: Option<&'static str>,
    outcome: Option<StreamOutcome>,
}

impl WsSession {
    /// A session for an upgrade request that reached the gateway.
    #[must_use]
    pub fn new(idle_window_secs: Option<u64>) -> Self {
        Self {
            state: WsState::Upgrading,
            bytes_downstream: 0,
            bytes_upstream: 0,
            idle_window_secs,
            error_type: None,
            outcome: None,
        }
    }

    /// The state the session is in.
    #[must_use]
    pub const fn state(&self) -> WsState {
        self.state
    }

    /// The outcome the terminal transition recorded, when the session ended.
    #[must_use]
    pub const fn outcome(&self) -> Option<&StreamOutcome> {
        self.outcome.as_ref()
    }

    /// Add the byte count of a frame relayed downstream
    /// (`inst-ss-rlb-04`).
    pub fn count_downstream(&mut self, bytes: u64) {
        self.bytes_downstream += bytes;
    }

    /// Add the byte count of a frame relayed upstream (`inst-ss-rlb-04`).
    pub fn count_upstream(&mut self, bytes: u64) {
        self.bytes_upstream += bytes;
    }

    /// Record the GTS `type` identifier of the failure the session ends in
    /// (`inst-ss-cls-10`).
    pub fn record_error(&mut self, error: &DomainError) {
        self.error_type = Some(error.gts_id());
    }

    /// Advance the session on `event`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `502` of a transition
    /// `cpt-cf-oagw-state-ws-session` does not declare, which is a programming
    /// fault of the relay rather than a caller fault: no state is skipped and a
    /// terminal state is never left.
    pub fn transition(&mut self, event: WsEvent) -> Result<(), DomainError> {
        if !Self::allows(self.state, event) {
            return Err(DomainError::ProtocolError {
                detail: format!(
                    "the websocket session cannot move from {} on this event",
                    self.state.as_str()
                ),
            });
        }
        self.state = Self::target(event);
        if self.state.is_terminal() {
            self.outcome = Some(self.outcome_of(event));
        }
        Ok(())
    }

    /// Whether the session allows the transition `state` takes on `event`.
    #[must_use]
    const fn allows(state: WsState, event: WsEvent) -> bool {
        use WsEvent::{
            AttemptFailed, Closed, Established, Failed, FrameRelayed, IdleElapsed, Refused,
        };
        matches!(
            (state, event),
            (WsState::Upgrading, Established)
                | (WsState::Upgrading, Refused)
                | (WsState::Upgrading, AttemptFailed)
                | (WsState::Established, FrameRelayed)
                | (WsState::Established, Closed { .. })
                | (WsState::Established, Failed { .. })
                | (WsState::Established, IdleElapsed)
                | (WsState::Relaying, Closed { .. })
                | (WsState::Relaying, Failed { .. })
                | (WsState::Relaying, IdleElapsed)
        )
    }

    /// The state the session reaches on `event`.
    const fn target(event: WsEvent) -> WsState {
        match event {
            WsEvent::Established => WsState::Established,
            WsEvent::Refused => WsState::Rejected,
            WsEvent::AttemptFailed => WsState::Aborted,
            WsEvent::FrameRelayed => WsState::Relaying,
            WsEvent::Closed { .. } => WsState::Closed,
            WsEvent::Failed { .. } => WsState::Aborted,
            WsEvent::IdleElapsed => WsState::TimedOut,
        }
    }

    /// The outcome the terminal transition records.
    fn outcome_of(&self, event: WsEvent) -> StreamOutcome {
        let (reason, closing_side) = match event {
            WsEvent::Closed { side } => {
                let reason = if side == SIDE_CLIENT {
                    CloseReason::ClientDisconnected
                } else {
                    CloseReason::UpstreamClosed
                };
                (reason, side)
            }
            WsEvent::Failed { side, reason } => (reason, side),
            WsEvent::IdleElapsed => (CloseReason::IdleTimeout, SIDE_NONE),
            WsEvent::Refused => (CloseReason::Rejected, SIDE_NONE),
            WsEvent::AttemptFailed => (CloseReason::Aborted, SIDE_NONE),
            WsEvent::Established | WsEvent::FrameRelayed => {
                (CloseReason::UpstreamClosed, SIDE_NONE)
            }
        };
        StreamOutcome::terminal(
            reason,
            closing_side,
            self.bytes_downstream,
            self.bytes_upstream,
            self.error_type,
            self.idle_window_secs,
        )
    }
}

/// The failure a streamed exchange raised
/// (`cpt-cf-oagw-algo-stream-error-classify` input).
#[derive(Debug, Clone)]
pub enum StreamFailure {
    /// The upstream exchange was lost while its body was being relayed: a
    /// reset, an abort, a mid-body loss or an abort before the first byte.
    UpstreamLoss,
    /// The exchange stayed silent past the idle window
    /// (`cpt-cf-oagw-algo-stream-timeout`).
    IdleElapsed,
    /// A plaintext upgrade was refused before any connection attempt.
    RefusedPlaintext,
    /// The upgrade request was malformed.
    MalformedUpgrade,
    /// The client went away: the one failure that produces no response at all
    /// (`cpt-cf-oagw-flow-stream-disconnect`).
    ClientDisconnect,
    /// The upgrade dial failed; entry 2.4's call classifier already mapped the
    /// row for that cause.
    Call(DomainError),
}

/// The decision the classifier returns for one stream failure.
#[derive(Debug, Clone)]
pub struct StreamErrorDecision {
    /// The row of the canonical error table the failure maps to
    /// (`inst-ss-cls-02`): no row is added.
    pub error: DomainError,
    /// Whether a `application/problem+json` response may still be written
    /// (`inst-ss-cls-01`): the head has not been committed and no body byte has
    /// been flushed.
    pub problem_writable: bool,
    /// The close reason the failure records.
    pub reason: CloseReason,
    /// The side that failed the exchange.
    pub closing_side: &'static str,
}

/// Classify a stream failure onto the canonical error table
/// (`cpt-cf-oagw-algo-stream-error-classify`).
///
/// The classifier is a pure function of the failure and of the two facts that
/// decide whether a problem document is still possible: whether the response
/// head has been committed to the client and whether a body byte has been
/// flushed (`inst-ss-cls-01`).
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamErrorClassifier;

impl StreamErrorClassifier {
    /// Map `failure` onto exactly one row of the error table
    /// (`inst-ss-cls-02`).
    ///
    /// An upstream reset, abort, mid-body loss or an abort before the first
    /// body byte maps to `502` StreamAborted; a silent exchange past the idle
    /// window maps to `504` IdleTimeout; a refused plaintext upgrade maps to
    /// `503` LinkUnavailable; a malformed upgrade maps to `400`
    /// ValidationError; a failed upgrade attempt maps to the row the entry-2.4
    /// call classifier already fixed for that cause.
    #[must_use]
    pub fn classify(
        failure: StreamFailure,
        head_committed: bool,
        body_flushed: bool,
    ) -> StreamErrorDecision {
        let client_gone = matches!(failure, StreamFailure::ClientDisconnect);
        let (reason, closing_side, error) = match failure {
            StreamFailure::UpstreamLoss => (
                CloseReason::Aborted,
                SIDE_UPSTREAM,
                DomainError::StreamAborted {
                    detail: "the upstream stream was lost while it was being relayed".to_owned(),
                },
            ),
            StreamFailure::IdleElapsed => (
                CloseReason::IdleTimeout,
                SIDE_NONE,
                DomainError::IdleTimeout {
                    detail: "the streamed exchange stayed silent past the idle window".to_owned(),
                    retry_after_seconds: None,
                },
            ),
            StreamFailure::RefusedPlaintext => (
                CloseReason::Rejected,
                SIDE_NONE,
                DomainError::LinkUnavailable {
                    detail: "the endpoint scheme is `http` and `allow_http_upstream` is `false`"
                        .to_owned(),
                    retry_after_seconds: None,
                },
            ),
            StreamFailure::MalformedUpgrade => (
                CloseReason::Rejected,
                SIDE_NONE,
                DomainError::ValidationError {
                    detail: "the websocket upgrade request is not well formed".to_owned(),
                },
            ),
            StreamFailure::ClientDisconnect => (
                CloseReason::ClientDisconnected,
                SIDE_CLIENT,
                DomainError::StreamAborted {
                    detail: "the caller went away while the exchange was streaming".to_owned(),
                },
            ),
            StreamFailure::Call(error) => (CloseReason::Aborted, SIDE_UPSTREAM, error),
        };
        // A mid-stream problem document is never spliced into a body the client
        // already receives as upstream bytes (`inst-ss-cls-07`), and a client
        // disconnect is never reported to a client that is gone
        // (`inst-ss-cdc-07`).
        StreamErrorDecision {
            problem_writable: !head_committed && !body_flushed && !client_gone,
            reason,
            closing_side,
            error,
        }
    }
}

/// The in-memory record of one streamed exchange (`inst-ss-lif-01`).
///
/// The record is what entry 2.7 reads about a stream: the stream kind, the
/// identity of the selected endpoint, the negotiated-upgrade facts as booleans
/// and the two lifecycles that carry the close reason, the failing direction,
/// the byte counts and the error type. No header value, no request body byte
/// and no query string is in it, and the record lives for one exchange only.
#[derive(Debug)]
pub struct StreamRecord {
    kind: StreamKind,
    endpoint: Option<String>,
    idle_window_secs: Option<u64>,
    exchange: Mutex<StreamLifecycle>,
    session: Option<Mutex<WsSession>>,
    /// Whether the upstream negotiated a subprotocol (`inst-ss-upg-19`): a
    /// boolean, never a header value.
    subprotocol: AtomicBool,
    /// Whether the upstream negotiated extensions: a boolean, never a value.
    extensions: AtomicBool,
}

impl StreamRecord {
    /// Open a record for an exchange the handoff declared as `kind`
    /// (`inst-ss-lif-01`).
    ///
    /// `endpoint` is the identity of the selected endpoint — the authority the
    /// pipeline dialed, never a client-supplied host.
    #[must_use]
    pub fn new(
        kind: StreamKind,
        endpoint: Option<String>,
        idle_window_secs: Option<u64>,
    ) -> Self {
        let session = (kind == StreamKind::WebSocket).then(|| Mutex::new(WsSession::new(idle_window_secs)));
        Self {
            kind,
            endpoint,
            idle_window_secs,
            exchange: Mutex::new(StreamLifecycle::new(idle_window_secs)),
            session,
            subprotocol: AtomicBool::new(false),
            extensions: AtomicBool::new(false),
        }
    }

    /// The kind the handoff declared the exchange to be.
    #[must_use]
    pub const fn kind(&self) -> StreamKind {
        self.kind
    }

    /// The identity of the selected endpoint, when the handoff carried one.
    #[must_use]
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// The idle window the exchange is armed with, in seconds
    /// (`inst-ss-tmo-01`).
    #[must_use]
    pub const fn idle_window_secs(&self) -> Option<u64> {
        self.idle_window_secs
    }

    /// Whether the upstream negotiated a subprotocol or an extension.
    ///
    /// The facts are booleans: no header value reaches the record.
    pub fn negotiated(&self, subprotocol: bool, extensions: bool) {
        self.subprotocol.store(subprotocol, Ordering::Relaxed);
        self.extensions.store(extensions, Ordering::Relaxed);
    }

    /// Whether a subprotocol was negotiated (`inst-ss-upg-19`).
    #[must_use]
    pub fn has_subprotocol(&self) -> bool {
        self.subprotocol.load(Ordering::Relaxed)
    }

    /// Whether an extension was negotiated.
    #[must_use]
    pub fn has_extensions(&self) -> bool {
        self.extensions.load(Ordering::Relaxed)
    }

    /// Advance the exchange lifecycle on `event` (`inst-ss-lif-02`).
    ///
    /// # Errors
    ///
    /// Returns the mapped `502` of a transition the state machine does not
    /// declare; the state is kept.
    pub fn advance(&self, event: StreamEvent) -> Result<(), DomainError> {
        self.exchange.lock().transition(event)
    }

    /// Advance the WebSocket session on `event`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `502` of a transition the state machine does not
    /// declare; the state is kept.
    pub fn advance_session(&self, event: WsEvent) -> Result<(), DomainError> {
        match &self.session {
            Some(session) => session.lock().transition(event),
            None => Err(DomainError::ProtocolError {
                detail: "the exchange carries no websocket session".to_owned(),
            }),
        }
    }

    /// Add the byte count of a chunk relayed downstream
    /// (`inst-ss-sse-11`).
    pub fn count_downstream(&self, bytes: u64) {
        self.exchange.lock().count_downstream(bytes);
    }

    /// Add the byte count of a chunk or frame received upstream
    /// (`inst-ss-rlb-04`).
    pub fn count_upstream(&self, bytes: u64) {
        self.exchange.lock().count_upstream(bytes);
    }

    /// Record the GTS `type` identifier of the failure on both lifecycles
    /// (`inst-ss-cls-10`).
    pub fn record_error(&self, error: &DomainError) {
        self.exchange.lock().record_error(error);
        if let Some(session) = &self.session {
            session.lock().record_error(error);
        }
    }

    /// The state the exchange lifecycle is in.
    #[must_use]
    pub fn state(&self) -> StreamState {
        self.exchange.lock().state()
    }

    /// The state the WebSocket session is in, when the exchange carries one.
    #[must_use]
    pub fn session_state(&self) -> Option<WsState> {
        self.session.as_ref().map(|session| session.lock().state())
    }

    /// The outcome the exchange lifecycle recorded, when the exchange ended.
    #[must_use]
    pub fn outcome(&self) -> Option<StreamOutcome> {
        self.exchange.lock().outcome().cloned()
    }

    /// Consume the record into the outcome it recorded.
    #[must_use]
    pub fn into_outcome(self) -> Option<StreamOutcome> {
        self.exchange.into_inner().into_outcome()
    }
}

/// The shared record one relay drives.
pub type SharedRecord = Arc<StreamRecord>;

/// The idle window of one streamed exchange, in seconds
/// (`cpt-cf-oagw-algo-stream-timeout`).
///
/// The window is read from the same `oagw.config.proxy_timeout_secs` value the
/// entry-2.4 call used, so no separate stream timeout configuration exists
/// (`inst-ss-tmo-01`).
#[must_use]
pub fn idle_window(timeout: Duration) -> Option<u64> {
    Some(timeout.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> StreamLifecycle {
        StreamLifecycle::new(Some(30))
    }

    fn session() -> WsSession {
        WsSession::new(Some(30))
    }

    #[test]
    fn a_stream_lifecycle_opens_in_the_handed_off_state() {
        let lifecycle = machine();
        assert_eq!(lifecycle.state(), StreamState::HandedOff);
        assert_eq!(StreamState::HandedOff.as_str(), "handed_off");
        assert!(lifecycle.outcome().is_none(), "nothing ended yet");
    }

    #[test]
    fn the_head_relay_and_the_first_chunk_walk_the_open_states() {
        let mut lifecycle = machine();
        lifecycle.transition(StreamEvent::HeadRelayed).expect("open");
        assert_eq!(lifecycle.state(), StreamState::Open);
        lifecycle.transition(StreamEvent::ChunkFlushed).expect("relaying");
        assert_eq!(lifecycle.state(), StreamState::Relaying);
        assert!(lifecycle.outcome().is_none(), "the exchange is still open");
    }

    #[test]
    fn a_clean_end_of_the_stream_closes_the_exchange_with_its_byte_counts() {
        let mut lifecycle = machine();
        lifecycle.transition(StreamEvent::HeadRelayed).expect("open");
        lifecycle.count_downstream(12);
        lifecycle.count_downstream(9);
        lifecycle.transition(StreamEvent::UpstreamEnded).expect("closed");
        assert_eq!(lifecycle.state(), StreamState::Closed);
        let outcome = lifecycle.outcome().expect("the close is recorded");
        assert_eq!(outcome.reason, CloseReason::UpstreamClosed);
        assert_eq!(outcome.reason.as_str(), "upstream_closed");
        assert_eq!(outcome.closing_side, SIDE_UPSTREAM);
        assert_eq!(outcome.bytes_downstream, 21);
        assert_eq!(outcome.bytes_upstream, 0);
        assert_eq!(outcome.error_type, None);
        assert_eq!(outcome.idle_window_secs, Some(30));
    }

    #[test]
    fn an_empty_body_ends_an_open_exchange_that_never_relayed_a_chunk() {
        // `inst-ss-stl-05`: the upstream ends the streamed response with an
        // empty body, so the exchange is closed from `open` directly.
        let mut lifecycle = machine();
        lifecycle.transition(StreamEvent::HeadRelayed).expect("open");
        lifecycle.transition(StreamEvent::UpstreamEnded).expect("closed");
        assert_eq!(lifecycle.state(), StreamState::Closed);
    }

    #[test]
    fn a_client_disconnect_aborts_the_exchange_without_an_error_type() {
        let mut lifecycle = machine();
        lifecycle.transition(StreamEvent::HeadRelayed).expect("open");
        lifecycle
            .transition(StreamEvent::Failed {
                side: SIDE_CLIENT,
                reason: CloseReason::ClientDisconnected,
            })
            .expect("aborted");
        assert_eq!(lifecycle.state(), StreamState::Aborted);
        let outcome = lifecycle.outcome().expect("the abort is recorded");
        assert_eq!(outcome.reason, CloseReason::ClientDisconnected);
        assert_eq!(outcome.reason.as_str(), "client_disconnected");
        assert_eq!(outcome.closing_side, SIDE_CLIENT);
    }

    #[test]
    fn an_upstream_abort_before_the_first_byte_is_recorded_from_the_handoff() {
        let mut lifecycle = machine();
        lifecycle.record_error(&DomainError::StreamAborted {
            detail: "lost".to_owned(),
        });
        lifecycle
            .transition(StreamEvent::Failed {
                side: SIDE_UPSTREAM,
                reason: CloseReason::Aborted,
            })
            .expect("aborted");
        assert_eq!(lifecycle.state(), StreamState::Aborted);
        let outcome = lifecycle.outcome().expect("the abort is recorded");
        assert_eq!(
            outcome.error_type,
            Some("gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1")
        );
        assert_eq!(outcome.closing_side, SIDE_UPSTREAM);
    }

    #[test]
    fn an_idle_teardown_records_the_window_that_fired() {
        // `inst-ss-stl-03`: the window elapses before the head is relayed.
        let mut lifecycle = machine();
        lifecycle
            .transition(StreamEvent::IdleElapsed)
            .expect("timed out");
        assert_eq!(lifecycle.state(), StreamState::TimedOut);
        let outcome = lifecycle.outcome().expect("the teardown is recorded");
        assert_eq!(outcome.reason, CloseReason::IdleTimeout);
        assert_eq!(outcome.reason.as_str(), "idle_timeout");
        assert_eq!(outcome.closing_side, SIDE_NONE);
        assert_eq!(outcome.idle_window_secs, Some(30));
    }

    #[test]
    fn a_terminal_state_is_never_left_and_no_state_is_skipped() {
        let mut lifecycle = machine();
        lifecycle.transition(StreamEvent::HeadRelayed).expect("open");
        lifecycle.transition(StreamEvent::UpstreamEnded).expect("closed");
        for event in [
            StreamEvent::HeadRelayed,
            StreamEvent::ChunkFlushed,
            StreamEvent::UpstreamEnded,
            StreamEvent::IdleElapsed,
            StreamEvent::Failed {
                side: SIDE_CLIENT,
                reason: CloseReason::ClientDisconnected,
            },
        ] {
            let error = lifecycle
                .transition(event)
                .expect_err("a terminal state is never left");
            assert_eq!(error.status(), 502, "{error}");
        }
        assert_eq!(lifecycle.state(), StreamState::Closed, "the state is kept");
    }

    #[test]
    fn the_first_chunk_cannot_be_flushed_before_the_head_is_relayed() {
        let mut lifecycle = machine();
        let error = lifecycle
            .transition(StreamEvent::ChunkFlushed)
            .expect_err("no state is skipped");
        assert_eq!(error.status(), 502, "{error}");
        assert_eq!(lifecycle.state(), StreamState::HandedOff);
    }

    #[test]
    fn every_declared_transition_of_the_stream_lifecycle_is_reachable() {
        // The ten transitions `cpt-cf-oagw-state-stream-lifecycle` declares are
        // the only ones possible; each is reached from the state it names.
        let mut from_handoff = machine();
        from_handoff.transition(StreamEvent::HeadRelayed).expect("stl-01");
        from_handoff.transition(StreamEvent::ChunkFlushed).expect("stl-04");
        from_handoff.transition(StreamEvent::UpstreamEnded).expect("stl-08");
        assert_eq!(from_handoff.state(), StreamState::Closed);

        let mut from_open = machine();
        from_open.transition(StreamEvent::HeadRelayed).expect("stl-01");
        from_open.transition(StreamEvent::UpstreamEnded).expect("stl-05");
        assert_eq!(from_open.state(), StreamState::Closed);

        let mut to_relaying = machine();
        to_relaying.transition(StreamEvent::HeadRelayed).expect("stl-01");
        to_relaying.transition(StreamEvent::ChunkFlushed).expect("stl-04");
        assert_eq!(to_relaying.state(), StreamState::Relaying);
        to_relaying.transition(StreamEvent::IdleElapsed).expect("stl-10");
        assert_eq!(to_relaying.state(), StreamState::TimedOut);

        let mut aborted = machine();
        aborted.transition(StreamEvent::HeadRelayed).expect("stl-01");
        aborted.transition(StreamEvent::ChunkFlushed).expect("stl-04");
        aborted
            .transition(StreamEvent::Failed {
                side: SIDE_CLIENT,
                reason: CloseReason::ClientDisconnected,
            })
            .expect("stl-09");
        assert_eq!(aborted.state(), StreamState::Aborted);
    }

    #[test]
    fn the_terminal_states_of_the_lifecycle_are_the_three_declared_ones() {
        for state in [
            StreamState::Closed,
            StreamState::Aborted,
            StreamState::TimedOut,
        ] {
            assert!(state.is_terminal(), "{} is terminal", state.as_str());
        }
        for state in [
            StreamState::HandedOff,
            StreamState::Open,
            StreamState::Relaying,
        ] {
            assert!(!state.is_terminal(), "{} is not terminal", state.as_str());
        }
    }

    #[test]
    fn a_session_starts_upgrading_and_is_established_by_a_101() {
        let mut session = session();
        assert_eq!(session.state(), WsState::Upgrading);
        assert_eq!(WsState::Upgrading.as_str(), "upgrading");
        session.transition(WsEvent::Established).expect("stw-01");
        assert_eq!(session.state(), WsState::Established);
    }

    #[test]
    fn a_refused_or_failed_upgrade_ends_the_session_before_any_frame() {
        let mut refused = session();
        refused.transition(WsEvent::Refused).expect("stw-02");
        assert_eq!(refused.state(), WsState::Rejected);
        let outcome = refused.outcome().expect("the refusal is recorded");
        assert_eq!(outcome.reason, CloseReason::Rejected);
        assert_eq!(outcome.reason.as_str(), "rejected");

        let mut failed = session();
        failed.record_error(&DomainError::DownstreamError {
            detail: "unreachable".to_owned(),
        });
        failed.transition(WsEvent::AttemptFailed).expect("stw-03");
        assert_eq!(failed.state(), WsState::Aborted);
        let outcome = failed.outcome().expect("the failure is recorded");
        assert_eq!(
            outcome.error_type,
            Some("gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1")
        );
    }

    #[test]
    fn a_session_that_relays_frames_closes_from_the_relaying_state() {
        let mut session = session();
        session.transition(WsEvent::Established).expect("stw-01");
        session.count_upstream(4);
        session.transition(WsEvent::FrameRelayed).expect("stw-04");
        assert_eq!(session.state(), WsState::Relaying);
        session.count_downstream(7);
        session
            .transition(WsEvent::Closed {
                side: SIDE_UPSTREAM,
            })
            .expect("stw-07");
        assert_eq!(session.state(), WsState::Closed);
        let outcome = session.outcome().expect("the close is recorded");
        assert_eq!(outcome.reason, CloseReason::UpstreamClosed);
        assert_eq!(outcome.closing_side, SIDE_UPSTREAM);
        assert_eq!(outcome.bytes_upstream, 4);
        assert_eq!(outcome.bytes_downstream, 7);
    }

    #[test]
    fn a_client_that_closes_first_is_recorded_as_the_closing_side() {
        let mut session = session();
        session.transition(WsEvent::Established).expect("stw-01");
        session.transition(WsEvent::FrameRelayed).expect("stw-04");
        session
            .transition(WsEvent::Closed {
                side: SIDE_CLIENT,
            })
            .expect("stw-07");
        let outcome = session.outcome().expect("the close is recorded");
        assert_eq!(outcome.reason, CloseReason::ClientDisconnected);
        assert_eq!(outcome.closing_side, SIDE_CLIENT);
    }

    #[test]
    fn a_relay_failure_aborts_the_session_and_records_the_error_type() {
        let mut session = session();
        session.transition(WsEvent::Established).expect("stw-01");
        session.transition(WsEvent::FrameRelayed).expect("stw-04");
        session.record_error(&DomainError::StreamAborted {
            detail: "reset".to_owned(),
        });
        session
            .transition(WsEvent::Failed {
                side: SIDE_UPSTREAM,
                reason: CloseReason::Aborted,
            })
            .expect("stw-08");
        assert_eq!(session.state(), WsState::Aborted);
        let outcome = session.outcome().expect("the abort is recorded");
        assert_eq!(outcome.reason, CloseReason::Aborted);
        assert_eq!(
            outcome.error_type,
            Some("gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1")
        );
    }

    #[test]
    fn an_idle_session_times_out_from_established_and_from_relaying() {
        let mut established = session();
        established.transition(WsEvent::Established).expect("stw-01");
        established.transition(WsEvent::IdleElapsed).expect("stw-10");
        assert_eq!(established.state(), WsState::TimedOut);

        let mut relaying = session();
        relaying.transition(WsEvent::Established).expect("stw-01");
        relaying.transition(WsEvent::FrameRelayed).expect("stw-04");
        relaying.transition(WsEvent::IdleElapsed).expect("stw-09");
        assert_eq!(relaying.state(), WsState::TimedOut);
        let outcome = relaying.outcome().expect("the teardown is recorded");
        assert_eq!(outcome.idle_window_secs, Some(30));
    }

    #[test]
    fn a_terminal_session_state_is_never_left_and_no_state_is_skipped() {
        let mut session = session();
        session.transition(WsEvent::Refused).expect("stw-02");
        for event in [
            WsEvent::Established,
            WsEvent::Refused,
            WsEvent::AttemptFailed,
            WsEvent::FrameRelayed,
            WsEvent::IdleElapsed,
            WsEvent::Closed {
                side: SIDE_CLIENT,
            },
            WsEvent::Failed {
                side: SIDE_CLIENT,
                reason: CloseReason::ClientDisconnected,
            },
        ] {
            session
                .transition(event)
                .expect_err("a terminal session is never left");
        }
        assert_eq!(session.state(), WsState::Rejected, "the state is kept");

        // The first frame cannot be relayed before the session is established.
        let mut fresh = WsSession::new(Some(30));
        let error = fresh
            .transition(WsEvent::FrameRelayed)
            .expect_err("no state is skipped");
        assert_eq!(error.status(), 502, "{error}");
    }

    #[test]
    fn the_terminal_states_of_the_session_are_the_four_declared_ones() {
        for state in [
            WsState::Rejected,
            WsState::Closed,
            WsState::Aborted,
            WsState::TimedOut,
        ] {
            assert!(state.is_terminal(), "{} is terminal", state.as_str());
        }
        for state in [
            WsState::Upgrading,
            WsState::Established,
            WsState::Relaying,
        ] {
            assert!(!state.is_terminal(), "{} is not terminal", state.as_str());
        }
    }

    #[test]
    fn the_stream_kinds_carry_the_wire_tokens_the_observability_layer_labels() {
        assert_eq!(StreamKind::SseResponse.as_str(), "sse_response");
        assert_eq!(StreamKind::SseRequest.as_str(), "sse_request");
        assert_eq!(StreamKind::WebSocket.as_str(), "websocket");
    }

    #[test]
    fn the_close_reasons_carry_the_wire_tokens_entry_2_7_labels() {
        assert_eq!(CloseReason::UpstreamClosed.as_str(), "upstream_closed");
        assert_eq!(CloseReason::ClientDisconnected.as_str(), "client_disconnected");
        assert_eq!(CloseReason::Aborted.as_str(), "aborted");
        assert_eq!(CloseReason::IdleTimeout.as_str(), "idle_timeout");
        assert_eq!(CloseReason::Rejected.as_str(), "rejected");
    }

    #[test]
    fn the_recorded_outcome_carries_no_credential_or_header_material() {
        let outcome = StreamOutcome::terminal(
            CloseReason::UpstreamClosed,
            SIDE_UPSTREAM,
            128,
            64,
            Some("gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"),
            Some(30),
        );
        let rendered = format!("{outcome:?}").to_lowercase();
        for material in [
            "bearer",
            "secret",
            "authorization",
            "sec-websocket",
            "connection",
            "upgrade",
            "content-type",
            "query",
        ] {
            assert!(!rendered.contains(material), "{material} leaked: {rendered}");
        }
    }

    #[test]
    fn the_stream_record_holds_the_kind_the_endpoint_and_the_lifecycles() {
        let record = StreamRecord::new(
            StreamKind::WebSocket,
            Some("a.vendor.com:443".to_owned()),
            Some(30),
        );
        assert_eq!(record.kind(), StreamKind::WebSocket);
        assert_eq!(record.kind().as_str(), "websocket");
        assert_eq!(record.endpoint(), Some("a.vendor.com:443"));
        assert_eq!(record.idle_window_secs(), Some(30));
        assert_eq!(record.state(), StreamState::HandedOff);
        assert_eq!(record.session_state(), Some(WsState::Upgrading));
        assert!(record.outcome().is_none());

        // An SSE record carries no session to advance.
        let sse = StreamRecord::new(StreamKind::SseResponse, None, None);
        assert!(sse.session_state().is_none());
        assert!(sse.advance_session(WsEvent::Established).is_err());
    }

    #[test]
    fn the_record_records_the_negotiated_upgrade_as_booleans_only() {
        let record = StreamRecord::new(
            StreamKind::WebSocket,
            Some("a.vendor.com:443".to_owned()),
            Some(30),
        );
        record.negotiated(true, false);
        assert!(record.has_subprotocol());
        assert!(!record.has_extensions());
    }

    #[test]
    fn the_record_advances_the_lifecycle_and_the_session_together() {
        let record = StreamRecord::new(
            StreamKind::WebSocket,
            Some("a.vendor.com:443".to_owned()),
            Some(30),
        );
        record.advance(StreamEvent::HeadRelayed).expect("open");
        record.advance_session(WsEvent::Established).expect("stw-01");
        record.advance_session(WsEvent::FrameRelayed).expect("stw-04");
        record.count_downstream(3);
        record
            .advance(StreamEvent::Failed {
                side: SIDE_CLIENT,
                reason: CloseReason::ClientDisconnected,
            })
            .expect("aborted");
        record
            .advance_session(WsEvent::Failed {
                side: SIDE_CLIENT,
                reason: CloseReason::ClientDisconnected,
            })
            .expect("aborted");
        assert_eq!(record.state(), StreamState::Aborted);
        assert_eq!(record.session_state(), Some(WsState::Aborted));
        let outcome = record.outcome().expect("the outcome is recorded");
        assert_eq!(outcome.reason, CloseReason::ClientDisconnected);
        assert_eq!(outcome.bytes_downstream, 3);
    }

    #[test]
    fn an_aborted_stream_maps_onto_the_existing_502_row() {
        let decision = StreamErrorClassifier::classify(
            StreamFailure::UpstreamLoss,
            false,
            false,
        );
        assert_eq!(decision.error.status(), 502, "{:?}", decision.error);
        assert_eq!(
            decision.error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
        );
        assert!(decision.problem_writable, "the head is still writable");
        assert_eq!(decision.reason, CloseReason::Aborted);
        assert_eq!(decision.closing_side, SIDE_UPSTREAM);
    }

    #[test]
    fn a_mid_stream_abort_writes_no_problem_document() {
        let decision = StreamErrorClassifier::classify(StreamFailure::UpstreamLoss, true, false);
        assert!(
            !decision.problem_writable,
            "a problem document is never spliced into a body the client already receives"
        );
        let decision = StreamErrorClassifier::classify(StreamFailure::UpstreamLoss, true, true);
        assert!(!decision.problem_writable);
        let decision = StreamErrorClassifier::classify(StreamFailure::UpstreamLoss, false, true);
        assert!(!decision.problem_writable);
    }

    #[test]
    fn a_silent_exchange_maps_onto_the_existing_504_row() {
        let decision =
            StreamErrorClassifier::classify(StreamFailure::IdleElapsed, false, false);
        assert_eq!(decision.error.status(), 504, "{:?}", decision.error);
        assert_eq!(
            decision.error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"
        );
        assert!(decision.problem_writable);
        assert_eq!(decision.reason, CloseReason::IdleTimeout);
    }

    #[test]
    fn a_refused_plaintext_upgrade_maps_onto_the_existing_503_row() {
        let decision = StreamErrorClassifier::classify(
            StreamFailure::RefusedPlaintext,
            false,
            false,
        );
        assert_eq!(decision.error.status(), 503, "{:?}", decision.error);
        assert_eq!(
            decision.error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
        assert!(decision.problem_writable);
        assert_eq!(decision.reason, CloseReason::Rejected);
    }

    #[test]
    fn a_malformed_upgrade_maps_onto_the_existing_400_row() {
        let decision = StreamErrorClassifier::classify(
            StreamFailure::MalformedUpgrade,
            false,
            false,
        );
        assert_eq!(decision.error.status(), 400, "{:?}", decision.error);
        assert_eq!(
            decision.error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert!(decision.problem_writable);
    }

    #[test]
    fn a_failed_upgrade_attempt_keeps_the_row_the_call_classifier_fixed() {
        let decision = StreamErrorClassifier::classify(
            StreamFailure::Call(DomainError::DownstreamError {
                detail: "unreachable".to_owned(),
            }),
            false,
            false,
        );
        assert_eq!(decision.error.status(), 502, "{:?}", decision.error);
        assert_eq!(
            decision.error.gts_id(),
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1"
        );
        assert!(decision.problem_writable);
    }

    #[test]
    fn a_client_disconnect_produces_no_response_at_all() {
        let decision = StreamErrorClassifier::classify(
            StreamFailure::ClientDisconnect,
            false,
            false,
        );
        assert!(
            !decision.problem_writable,
            "the abort is recorded, not reported to a client that is gone"
        );
        assert_eq!(decision.reason, CloseReason::ClientDisconnected);
        assert_eq!(decision.closing_side, SIDE_CLIENT);
    }

    #[test]
    fn the_idle_window_is_the_value_the_call_used() {
        assert_eq!(idle_window(Duration::from_secs(30)), Some(30));
        assert_eq!(idle_window(Duration::from_secs(1)), Some(1));
    }
}
