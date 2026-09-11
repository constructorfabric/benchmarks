//! The data-plane types of entry 2.4
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`).
//!
//! The proxy handler of `/oagw/v1/proxy/{alias}[/{path_suffix}]` speaks in
//! these values and nothing else: it builds a [`ProxyContext`] from the
//! inbound request, hands it to the [`crate::domain::services::proxy::DataPlaneService`]
//! seam, and renders the returned [`ProxyResponse`] or [`ProxyFailure`]. The
//! HTTP framework, the HTTP client and the transport stay in `api` and `infra`
//! — the domain layer names no `axum`, `hyper` or `toolkit-http` type.
//!
//! Two properties the whole entry is measured by are carried here rather than
//! by any implementation detail:
//!
//! * **error-source attribution** ([`ErrorSource`]) — every response the
//!   pipeline produces is classified gateway-generated or upstream-passthrough
//!   (`cpt-cf-oagw-adr-error-source-distinction`), which is the one thing this
//!   entry owns of the shared error contract;
//! * **the pipeline-boundary observation** ([`ProxyObservation`]) — `status`,
//!   `duration_ms`, `request_size`, `response_size` and `error_type` are made
//!   available and *nothing* is emitted: audit records, metrics and trace
//!   identifiers are owned by `cpt-cf-oagw-feature-observability-and-operability`.

use std::pin::Pin;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::Stream;
use uuid::Uuid;

use crate::domain::cors::CorsObservation;
use crate::domain::error::DomainError;

/// The transport kind a proxy exchange is carrying
/// (`cpt-cf-oagw-algo-request-proxy-stream-lifecycle` step 1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StreamKind {
    /// A buffered request/response exchange.
    #[default]
    None,
    /// `text/event-stream`, relayed event by event.
    Sse,
    /// A WebSocket session relayed bidirectionally.
    WebSocket,
    /// A WebTransport session relayed bidirectionally.
    WebTransport,
}

impl StreamKind {
    /// Whether the exchange is a streaming session rather than a buffered one.
    #[must_use]
    pub const fn is_streaming(self) -> bool {
        !matches!(self, Self::None)
    }

    /// The `X-OAGW-Error-Source`-carrying value the lifecycle records name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "buffered",
            Self::Sse => "sse",
            Self::WebSocket => "ws",
            Self::WebTransport => "wt",
        }
    }
}

/// Which side produced a response
/// (`cpt-cf-oagw-adr-error-source-distinction`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ErrorSource {
    /// OAGW generated the response: the body is the problem+json contract of
    /// entry 2.5.
    #[default]
    Gateway,
    /// The upstream status and body pass through unmodified.
    Upstream,
}

impl ErrorSource {
    /// The `X-OAGW-Error-Source` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// One lifecycle event of a streaming session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEvent {
    /// The upstream accepted the session.
    Open,
    /// The session ended cleanly on either side.
    Close,
    /// The session failed before it completed.
    Aborted,
    /// The upstream refused or could not establish the session.
    Refused,
}

impl StreamEvent {
    /// The recorded event name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::Aborted => "aborted",
            Self::Refused => "refused",
        }
    }
}

/// The lifecycle record of a streaming session.
///
/// The relay task runs after the response head has been handed to the client,
/// so the outcome is shared with the response through this handle instead of
/// being returned: the events are recorded, never emitted — no metric, no
/// audit record and no trace identifier is produced here.
#[derive(Default)]
pub struct StreamLifecycle {
    events: Mutex<Vec<StreamEvent>>,
}

impl StreamLifecycle {
    /// An empty record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A shared handle for a response and its relay task.
    #[must_use]
    pub fn shared() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }

    /// Record one event, in arrival order.
    pub fn record(&self, event: StreamEvent) {
        self.events.lock().expect("stream lifecycle lock").push(event);
    }

    /// The events recorded so far, in arrival order.
    #[must_use]
    pub fn events(&self) -> Vec<StreamEvent> {
        self.events.lock().expect("stream lifecycle lock").clone()
    }

    /// Whether `event` was recorded.
    #[must_use]
    pub fn contains(&self, event: StreamEvent) -> bool {
        self.events().contains(&event)
    }
}

impl std::fmt::Debug for StreamLifecycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let events = self.events();
        formatter
            .debug_struct("StreamLifecycle")
            .field("events", &events.iter().map(|e| e.as_str()).collect::<Vec<_>>())
            .finish()
    }
}

/// The relayed body of a proxy response.
///
/// A buffered exchange carries the complete upstream body; a streaming
/// exchange carries a byte stream the handler wraps into the client response
/// without buffering it.
#[derive(Default)]
pub enum ProxyBody {
    /// No body (`204`, a `101` upgrade, a `HEAD`).
    #[default]
    Empty,
    /// The complete body, already read from the upstream.
    Buffered(Bytes),
    /// The body as it arrives, never buffered in full.
    Stream(ProxyByteStream),
}

impl std::fmt::Debug for ProxyBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("Empty"),
            Self::Buffered(bytes) => write!(formatter, "Buffered({} bytes)", bytes.len()),
            Self::Stream(_) => formatter.write_str("Stream(..)"),
        }
    }
}

/// The byte stream a streaming response relays.
///
/// An item is a chunk of the upstream body in arrival order; the error is the
/// domain error the abort is attributed with, which is what the
/// `X-OAGW-Error-Source` header of a mid-stream failure names.
pub type ProxyByteStream = Pin<Box<dyn Stream<Item = Result<bytes::Bytes, DomainError>> + Send>>;

/// The request the proxy dispatch hands to the data plane.
///
/// The header names are already lowercased by the HTTP framework; the body is
/// *not* validated here — body validation is a pipeline step of the service,
/// not of the extraction.
#[derive(Debug, Clone)]
pub struct ProxyContext {
    /// The request method.
    pub method: String,
    /// The requested alias, exactly as addressed, before normalization.
    pub alias: String,
    /// The path after the alias, with its leading slash; `None` when the
    /// request addressed `/oagw/v1/proxy/{alias}` with no suffix.
    pub path_suffix: Option<String>,
    /// The raw query string, without the `?`.
    pub query: Option<String>,
    /// The inbound headers, in arrival order, including `host` and the
    /// routing headers.
    pub headers: Vec<(String, String)>,
    /// The inbound body, read before the pipeline runs.
    pub body: Bytes,
    /// The calling tenant, from the security context.
    pub tenant_id: Uuid,
    /// The calling principal, from the security context.
    pub principal_id: Uuid,
    /// The connection peer address of the downstream client, taken from the
    /// connection and never from a client-supplied forwarding header.
    pub peer_addr: Option<String>,
    /// The inbound trace identifier, carried but never minted here.
    pub trace_id: Option<String>,
}

impl ProxyContext {
    /// The proxy request path the route matcher sees: the suffix with its
    /// leading slash, or `/` when no suffix was supplied.
    #[must_use]
    pub fn request_path(&self) -> String {
        match &self.path_suffix {
            Some(suffix) if !suffix.is_empty() => suffix.clone(),
            _ => "/".to_owned(),
        }
    }
}

/// The response the proxy pipeline returns to the handler.
///
/// The handler applies exactly two things to it: the `X-OAGW-Error-Source`
/// header from [`Self::source`], and — when [`Self::error`] is set — the
/// problem+json body of the shared error contract. Nothing else is added,
/// because an upstream response passes through unmodified.
pub struct ProxyResponse {
    /// The upstream status, or the status of the session opening.
    pub status: u16,
    /// The response headers, in order, after the response rules.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: ProxyBody,
    /// Which side produced the response.
    pub source: ErrorSource,
    /// The transport kind the exchange carried.
    pub stream: StreamKind,
    /// The lifecycle record of a streaming session.
    pub lifecycle: std::sync::Arc<StreamLifecycle>,
    /// The gateway error to render instead of the carried body.
    pub error: Option<DomainError>,
    /// The pipeline-boundary observation entry 2.9 consumes.
    pub observation: ProxyObservation,
}

impl ProxyResponse {
    /// A gateway response for `error`, with no upstream involved.
    #[must_use]
    pub fn gateway_error(error: DomainError, status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: ProxyBody::Empty,
            source: ErrorSource::Gateway,
            stream: StreamKind::None,
            lifecycle: StreamLifecycle::shared(),
            error: Some(error),
            observation: ProxyObservation::default(),
        }
    }

    /// The permissive `204` preflight answer the built-in CORS handler builds
    /// at the proxy handler (`cpt-cf-oagw-flow-cors-preflight`): gateway
    /// sourced, an empty body, and the preflight header set of
    /// [`crate::domain::cors::preflight_response_headers`].
    #[must_use]
    pub fn preflight(headers: Vec<(String, String)>) -> Self {
        Self {
            status: 204,
            headers,
            body: ProxyBody::Empty,
            source: ErrorSource::Gateway,
            stream: StreamKind::None,
            lifecycle: StreamLifecycle::shared(),
            error: None,
            observation: ProxyObservation {
                cors: Some(crate::domain::cors::CorsObservation {
                    outcome: crate::domain::cors::CorsOutcome::PreflightShortCircuit,
                }),
                ..ProxyObservation::default()
            },
        }
    }

    /// An upstream passthrough response of `status`.
    #[must_use]
    pub fn upstream(status: u16, headers: Vec<(String, String)>, body: ProxyBody) -> Self {
        Self {
            status,
            headers,
            body,
            source: ErrorSource::Upstream,
            stream: StreamKind::None,
            lifecycle: StreamLifecycle::shared(),
            error: None,
            observation: ProxyObservation::default(),
        }
    }
}

impl std::fmt::Debug for ProxyResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProxyResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &self.body)
            .field("source", &self.source.as_str())
            .field("stream", &self.stream.as_str())
            .field("lifecycle", &self.lifecycle)
            .field("error", &self.error.is_some())
            .field("observation", &self.observation)
            .finish()
    }
}

/// The proxy-path outcome entry 2.9 consumes at the pipeline boundary.
///
/// No value here is emitted by this feature: it is exposed and nothing more.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyObservation {
    /// The status the client received.
    pub status: u16,
    /// The wall-clock duration of the exchange.
    pub duration_ms: u64,
    /// The inbound body size in bytes.
    pub request_size: u64,
    /// The upstream body size in bytes, or `0` for a stream.
    pub response_size: u64,
    /// The GTS error type of a gateway failure.
    pub error_type: Option<&'static str>,
    /// The rate-limit outcome of the decision that served the request, and
    /// `None` when no `rate_limit` was configured for it
    /// (`cpt-cf-oagw-flow-rate-limiting-usage-observation`).
    pub rate_limit: Option<RateLimitObservation>,
    /// The CORS outcome the built-in CORS handler of entry 2.8 produced, and
    /// `None` when the exchange produced no CORS outcome of its own
    /// (`cpt-cf-oagw-flow-cors-preflight`).
    pub cors: Option<CorsObservation>,
    /// The upstream alias, which is the `host` label of the request families,
    /// and `None` before the alias is resolved
    /// (`cpt-cf-oagw-dod-observability-and-state-metric-cardinality`).
    pub host: Option<String>,
    /// The normalized route match pattern, which is the `http.route` label, and
    /// `None` for a request the route match rejected
    /// (`inst-os-algo-label-1`).
    pub route: Option<String>,
    /// The endpoint selection the upstream call performed, and `None` when the
    /// exchange reached no endpoint.
    pub routing: Option<RoutingObservation>,
    /// The per-phase durations of the pipeline, in milliseconds.
    pub phases: PhaseObservation,
}

/// The endpoint selection one upstream call performed, for the routing pair of
/// families (`inst-os-req-7`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingObservation {
    /// The `upstream_id` label.
    pub upstream_id: String,
    /// The `endpoint_host` label.
    pub endpoint_host: String,
    /// The `selection_method` label, as the enum the label value renders from.
    pub selection_method: crate::domain::endpoints::SelectionMethod,
    /// The host the target header was set from, when it was.
    pub target_host_used: bool,
}

impl Default for RoutingObservation {
    fn default() -> Self {
        Self {
            upstream_id: String::new(),
            endpoint_host: String::new(),
            selection_method: crate::domain::endpoints::SelectionMethod::Default,
            target_host_used: false,
        }
    }
}

/// The five per-phase durations of one pipeline run, in milliseconds
/// (`inst-os-req-2`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PhaseObservation {
    /// The route match step.
    pub route_match_ms: Option<u64>,
    /// The request-phase plugin chain.
    pub plugin_chain_request_ms: Option<u64>,
    /// The upstream call.
    pub upstream_call_ms: Option<u64>,
    /// The response-phase plugin chain.
    pub plugin_chain_response_ms: Option<u64>,
    /// The response assembly step.
    pub response_ms: Option<u64>,
}

/// The rate-limit outcome one decision produced, for the metrics recorder
/// owned by entry 2.9 (`inst-rl-obs-1` .. `-4`, `inst-rl-ratio-1` .. `-4`).
///
/// The consumed fraction is carried in parts per million rather than as a
/// floating-point value, so the observation stays an exact, comparable record;
/// the recorder renders `usage_ratio_parts_per_million / 1_000_000.0`. No
/// tenant label is carried, so the cardinality of the series stays bounded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RateLimitObservation {
    /// The `host` label: the upstream alias, never the raw request path.
    pub host: String,
    /// The `path` label: the normalized route match pattern.
    pub path: String,
    /// Whether the decision refused the request.
    pub refused: bool,
    /// The consumed fraction of the counter, in parts per million.
    pub usage_ratio_parts_per_million: u64,
    /// The retry guidance of a refused request, in seconds.
    pub retry_after_seconds: Option<u64>,
}

/// Why the proxy pipeline produced no upstream response.
///
/// Most failures are [`DomainError`] values, which entry 2.5 maps through the
/// shared table. A matched route whose method allowlist excludes the request
/// method is not a row of that table, so it is carried separately and rendered
/// as `405 Method Not Allowed` with an `Allow` header.
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyFailure {
    /// A domain rejection; entry 2.5 maps it onto its status and GTS type.
    Domain(DomainError),
    /// A path-matched route whose `match.http.methods` excludes the method.
    MethodNotAllowed {
        /// The proxy request path, for the `instance` field.
        path: Option<String>,
        /// The methods the matching routes admit, for the `Allow` header.
        allowed: Vec<&'static str>,
    },
}

impl From<DomainError> for ProxyFailure {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl ProxyFailure {
    /// The domain error of a domain failure, if it is one.
    #[must_use]
    pub const fn domain(&self) -> Option<&DomainError> {
        match self {
            Self::Domain(error) => Some(error),
            Self::MethodNotAllowed { .. } => None,
        }
    }
}
