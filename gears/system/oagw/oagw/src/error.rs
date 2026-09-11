//! Shared OAGW error model
//! (`cpt-cf-oagw-dod-problem-details-shape`,
//! `cpt-cf-oagw-dod-error-status-mapping`,
//! `cpt-cf-oagw-dod-error-source-header`).
//!
//! Every gateway-originated error is rendered as an RFC 9457
//! `application/problem+json` document carrying a fixed, documented
//! error-type-to-status mapping, plus the `X-OAGW-Error-Source` response
//! header. `toolkit_canonical_errors::Problem` is reused for the wire shape
//! (serialization, `application/problem+json` content type, status-code
//! conversion) rather than hand-rolling a body type.
//!
//! `toolkit_canonical_errors::CanonicalError` is intentionally **not** used
//! as the source type here: its 16 AIP-193 categories each carry a fixed
//! `cf.core.err.*` GTS suffix, but this gear's error-type-to-status table
//! (DESIGN.md) requires OAGW-specific GTS identifiers (e.g.
//! `cf.oagw.route.not_found.v1`) that the canonical category set cannot
//! express. [`OagwError`] builds a [`Problem`] directly instead.

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use toolkit_canonical_errors::Problem;
use toolkit_gts::gts_id;

/// Response header stamping response origin on every OAGW response
/// (`cpt-cf-oagw-dod-error-source-header`).
pub static ERROR_SOURCE_HEADER: HeaderName = HeaderName::from_static("x-oagw-error-source");

/// `X-OAGW-Error-Source` value for gateway-originated responses.
const SOURCE_GATEWAY: HeaderValue = HeaderValue::from_static("gateway");

/// `X-OAGW-Error-Source` value for passed-through upstream responses.
const SOURCE_UPSTREAM: HeaderValue = HeaderValue::from_static("upstream");

/// Fallback RFC 9457 `instance` value used only when a caller builds an
/// [`OagwError`] without attaching the request URI via
/// [`OagwError::with_instance`]. Every code path in this crate that renders
/// an error to a response is expected to set a real instance.
const UNSET_INSTANCE: &str = "about:blank";

/// One of the documented OAGW error types
/// (`cpt-cf-oagw-dod-error-status-mapping`), each mapped to a fixed HTTP
/// status and GTS `type` identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OagwErrorKind {
    /// `400` — request validation failed.
    ValidationError,
    /// `401` — authentication to upstream failed.
    AuthenticationFailed,
    /// `404` — no matching route found.
    RouteNotFound,
    /// `404` — no upstream with the given identifier exists for the calling
    /// tenant (`cpt-cf-oagw-dod-get-upstream-endpoint`,
    /// `cpt-cf-oagw-dod-replace-upstream-endpoint`,
    /// `cpt-cf-oagw-dod-delete-upstream-endpoint`). Not part of DESIGN.md's
    /// system-wide error-type table (which covers proxy-time errors); added
    /// here as the management-plane counterpart, following the same
    /// GTS-typed `Problem` shape.
    UpstreamNotFound,
    /// `409` — the derived or supplied alias already exists for the calling
    /// tenant (`cpt-cf-oagw-dod-alias-uniqueness`). Not part of DESIGN.md's
    /// system-wide error-type table (whose sole `409`, `PluginInUse`, is
    /// unrelated); added here as this feature's alias-uniqueness conflict.
    AliasConflict,
    /// `404` — no route with the given identifier exists for the calling
    /// tenant (`cpt-cf-oagw-dod-route-tenant-scoping`). The management-plane
    /// counterpart of `UpstreamNotFound`; deliberately distinct from
    /// `RouteNotFound`, which is DESIGN.md's proxy-time "no matching route"
    /// error (`cpt-cf-oagw-feature-http-proxy`), not a route CRUD lookup
    /// failure.
    RouteRecordNotFound,
    /// `409` — a create or replace candidate route's `path`, `priority`,
    /// and method set duplicate another enabled route under the same
    /// `upstream_id` (`cpt-cf-oagw-dod-route-conflict-detection`). Not part
    /// of DESIGN.md's system-wide error-type table (whose sole `409`,
    /// `PluginInUse`, is unrelated); added here as this feature's
    /// duplicate-match conflict.
    RouteConflict,
    /// `413` — request payload exceeds the configured limit.
    PayloadTooLarge,
    /// `429` — rate limit exceeded.
    RateLimitExceeded,
    /// `500` — referenced secret not found.
    SecretNotFound,
    /// `502` — upstream service error.
    DownstreamError,
    /// `503` — circuit breaker open.
    CircuitBreakerOpen,
    /// `504` — connection to the upstream timed out.
    ConnectionTimeout,
    /// `504` — the request to the upstream timed out.
    RequestTimeout,
    /// `504` — the connection to the upstream went idle past its deadline.
    IdleTimeout,
    /// `404` — no plugin with the given identifier exists for the calling
    /// tenant (`cpt-cf-oagw-dod-plugin-identification`). Deliberately not
    /// named `PluginNotFound`: that name is reserved for DESIGN.md's `503`
    /// runtime plugin-reference-resolution failure
    /// (`cpt-cf-oagw-feature-plugin-runtime`), a different failure mode from
    /// this management-plane CRUD lookup.
    PluginRecordNotFound,
    /// `409` — a create request's `name` already exists for the calling
    /// tenant, regardless of plugin type
    /// (`cpt-cf-oagw-dod-plugin-name-uniqueness`).
    PluginNameConflict,
    /// `409` — a delete request named a plugin still referenced by at least
    /// one upstream or route plugin binding
    /// (`cpt-cf-oagw-dod-plugin-delete`, `cpt-cf-oagw-adr-request-routing`).
    PluginInUse,
    /// `503` — every tenant tier carrying the requested alias holds only
    /// upstreams whose `enabled` field is false
    /// (`cpt-cf-oagw-dod-upstream-disabled-outcome`). DESIGN.md's
    /// `LinkUnavailable` proxy-time error type; the feature that renders it
    /// on the wire is `cpt-cf-oagw-feature-http-proxy`, not this one, but
    /// the kind is defined here alongside every other documented error type.
    LinkUnavailable,
    /// `400` — a multi-endpoint, common-suffix-alias upstream received no
    /// `X-OAGW-Target-Host` header (`cpt-cf-oagw-dod-target-host-selection`).
    MissingTargetHost,
    /// `400` — the supplied `X-OAGW-Target-Host` value is not a bare
    /// hostname or IP address (`cpt-cf-oagw-dod-target-host-selection`).
    InvalidTargetHost,
    /// `400` — the supplied `X-OAGW-Target-Host` value matches no configured
    /// endpoint host (`cpt-cf-oagw-dod-target-host-selection`).
    UnknownTargetHost,
    /// `502` — an upgrade handshake answered with a status other than `101`,
    /// or the selected endpoint's scheme cannot serve the requested
    /// transport (`cpt-cf-oagw-dod-plaintext-connection-policy`,
    /// `cpt-cf-oagw-algo-upstream-invocation`). Never used for a plaintext
    /// refusal, which is always `LinkUnavailable` instead.
    ProtocolError,
    /// `403` — an actual cross-origin request's `Origin` does not match the
    /// effective CORS configuration's `allowed_origins`
    /// (`cpt-cf-oagw-dod-cors-request-enforcement`).
    CorsOriginNotAllowed,
    /// `403` — an actual cross-origin request's method is absent from the
    /// effective CORS configuration's `allowed_methods`
    /// (`cpt-cf-oagw-dod-cors-request-enforcement`).
    CorsMethodNotAllowed,
    /// `503` — a plugin binding reference could not be resolved to an
    /// executable implementation, or its configuration is structurally
    /// invalid for its plugin kind (`cpt-cf-oagw-dod-runtime-plugin-resolution`,
    /// `cpt-cf-oagw-dod-credential-isolation`). Distinct from
    /// `PluginRecordNotFound` (`404`), the management-plane CRUD lookup
    /// failure.
    PluginNotFound,
    /// `502` — an SSE upstream stream aborted after the response head
    /// arrived (and was recognised as an event stream) but before any body
    /// byte reached the caller (`cpt-cf-oagw-dod-sse-abort-handling`,
    /// CONS-F-001). Deliberately NOT used for two adjacent cases: a failure
    /// before the response head arrives at all keeps the shared transport
    /// classification (`DownstreamError`/`ConnectionTimeout`/`RequestTimeout`),
    /// since the response type isn't known yet; a failure after the first
    /// body byte has already reached the caller just ends the body, since
    /// the status has already committed and can no longer change.
    StreamAborted,
}

impl OagwErrorKind {
    /// The fixed HTTP status for this error type
    /// (`cpt-cf-oagw-dod-error-status-mapping`). Timeouts are three distinct
    /// types, all mapped to `504` — there is no generic `Timeout` type.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::ValidationError => StatusCode::BAD_REQUEST,
            Self::AuthenticationFailed => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound | Self::UpstreamNotFound | Self::RouteRecordNotFound => {
                StatusCode::NOT_FOUND
            }
            Self::AliasConflict
            | Self::RouteConflict
            | Self::PluginNameConflict
            | Self::PluginInUse => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound => StatusCode::INTERNAL_SERVER_ERROR,
            Self::DownstreamError | Self::ProtocolError | Self::StreamAborted => {
                StatusCode::BAD_GATEWAY
            }
            Self::CircuitBreakerOpen | Self::LinkUnavailable | Self::PluginNotFound => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
            Self::PluginRecordNotFound => StatusCode::NOT_FOUND,
            Self::MissingTargetHost | Self::InvalidTargetHost | Self::UnknownTargetHost => {
                StatusCode::BAD_REQUEST
            }
            Self::CorsOriginNotAllowed | Self::CorsMethodNotAllowed => StatusCode::FORBIDDEN,
        }
    }

    /// The GTS `type` identifier for this error's problem document, exactly
    /// as tabulated in DESIGN.md's error-type table (e.g.
    /// `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`).
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::ValidationError => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1")
            }
            Self::AuthenticationFailed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1")
            }
            Self::RouteNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
            }
            Self::UpstreamNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1")
            }
            Self::AliasConflict => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1")
            }
            Self::RouteRecordNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.route.record_not_found.v1")
            }
            Self::RouteConflict => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.route.conflict.v1")
            }
            Self::PayloadTooLarge => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1")
            }
            Self::RateLimitExceeded => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
            }
            Self::SecretNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1")
            }
            Self::DownstreamError => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1")
            }
            Self::CircuitBreakerOpen => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1")
            }
            Self::ConnectionTimeout => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1")
            }
            Self::RequestTimeout => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1")
            }
            Self::IdleTimeout => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1")
            }
            Self::PluginRecordNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.record_not_found.v1")
            }
            Self::PluginNameConflict => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.name_conflict.v1")
            }
            Self::PluginInUse => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1")
            }
            Self::LinkUnavailable => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1")
            }
            Self::MissingTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1")
            }
            Self::InvalidTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1")
            }
            Self::UnknownTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1")
            }
            Self::ProtocolError => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1")
            }
            Self::CorsOriginNotAllowed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1")
            }
            Self::CorsMethodNotAllowed => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1")
            }
            Self::PluginNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1")
            }
            Self::StreamAborted => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1")
            }
        }
    }

    /// Human-readable RFC 9457 `title` for this error type.
    // reason: this match deliberately keeps one arm per `OagwErrorKind`
    // variant, mirroring DESIGN.md's error-type table row-for-row so each
    // kind's title stays trivially auditable against it.
    // `RouteNotFound`/`RouteRecordNotFound` and `PluginRecordNotFound`/
    // `PluginNotFound` are documented (see their doc comments above) as
    // deliberately distinct kinds — different GTS types and status codes —
    // that merely happen to share the same human-readable title text; merging
    // their arms here would obscure that each kind still has its own
    // documented row.
    #[allow(clippy::match_same_arms)]
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::ValidationError => "Validation Error",
            Self::AuthenticationFailed => "Authentication Failed",
            Self::RouteNotFound => "Route Not Found",
            Self::UpstreamNotFound => "Upstream Not Found",
            Self::AliasConflict => "Alias Conflict",
            Self::RouteRecordNotFound => "Route Not Found",
            Self::RouteConflict => "Route Conflict",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::DownstreamError => "Downstream Error",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
            Self::PluginRecordNotFound => "Plugin Not Found",
            Self::PluginNameConflict => "Plugin Name Conflict",
            Self::PluginInUse => "Plugin In Use",
            Self::LinkUnavailable => "Link Unavailable",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::ProtocolError => "Protocol Error",
            Self::CorsOriginNotAllowed => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed => "CORS Method Not Allowed",
            Self::PluginNotFound => "Plugin Not Found",
            Self::StreamAborted => "Stream Aborted",
        }
    }
}

/// A single OAGW error occurrence: its documented type, human-readable
/// detail, and any applicable RFC 9457 extension fields
/// (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`)
/// (`cpt-cf-oagw-dod-problem-details-shape`).
#[derive(Debug, Clone)]
pub struct OagwError {
    kind: OagwErrorKind,
    detail: String,
    instance: Option<String>,
    upstream_id: Option<String>,
    host: Option<String>,
    path: Option<String>,
    retry_after_seconds: Option<u64>,
    trace_id: Option<String>,
    plugin_id: Option<String>,
    referenced_upstreams: Option<Vec<String>>,
    referenced_routes: Option<Vec<String>>,
    invalid_value: Option<String>,
    valid_hosts: Option<Vec<String>>,
    error_code: Option<String>,
    /// Raw response headers applied to the rendered response in addition to
    /// the problem-document body (`cpt-cf-oagw-dod-rate-limit-response`):
    /// e.g. `Retry-After`, `X-RateLimit-*`. Never used to carry a resolved
    /// credential, token, or secret reference value
    /// (`cpt-cf-oagw-dod-credential-isolation`).
    extra_headers: Vec<(HeaderName, HeaderValue)>,
}

impl OagwError {
    /// Builds an error of the given kind with the supplied detail message.
    #[must_use]
    pub fn new(kind: OagwErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            instance: None,
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            trace_id: None,
            plugin_id: None,
            referenced_upstreams: None,
            referenced_routes: None,
            invalid_value: None,
            valid_hosts: None,
            error_code: None,
            extra_headers: Vec::new(),
        }
    }

    /// `ValidationError` (`400`).
    #[must_use]
    pub fn validation_error(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::ValidationError, detail)
    }

    /// `AuthenticationFailed` (`401`).
    #[must_use]
    pub fn authentication_failed(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::AuthenticationFailed, detail)
    }

    /// `RouteNotFound` (`404`).
    #[must_use]
    pub fn route_not_found(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::RouteNotFound, detail)
    }

    /// `UpstreamNotFound` (`404`).
    #[must_use]
    pub fn upstream_not_found(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::UpstreamNotFound, detail)
    }

    /// `AliasConflict` (`409`).
    #[must_use]
    pub fn alias_conflict(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::AliasConflict, detail)
    }

    /// `RouteRecordNotFound` (`404`).
    #[must_use]
    pub fn route_record_not_found(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::RouteRecordNotFound, detail)
    }

    /// `RouteConflict` (`409`).
    #[must_use]
    pub fn route_conflict(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::RouteConflict, detail)
    }

    /// `PayloadTooLarge` (`413`).
    #[must_use]
    pub fn payload_too_large(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::PayloadTooLarge, detail)
    }

    /// `RateLimitExceeded` (`429`).
    #[must_use]
    pub fn rate_limit_exceeded(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::RateLimitExceeded, detail)
    }

    /// `SecretNotFound` (`500`).
    #[must_use]
    pub fn secret_not_found(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::SecretNotFound, detail)
    }

    /// `DownstreamError` (`502`).
    #[must_use]
    pub fn downstream_error(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::DownstreamError, detail)
    }

    /// `CircuitBreakerOpen` (`503`).
    #[must_use]
    pub fn circuit_breaker_open(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::CircuitBreakerOpen, detail)
    }

    /// `ConnectionTimeout` (`504`).
    #[must_use]
    pub fn connection_timeout(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::ConnectionTimeout, detail)
    }

    /// `RequestTimeout` (`504`).
    #[must_use]
    pub fn request_timeout(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::RequestTimeout, detail)
    }

    /// `IdleTimeout` (`504`).
    #[must_use]
    pub fn idle_timeout(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::IdleTimeout, detail)
    }

    /// `PluginRecordNotFound` (`404`).
    #[must_use]
    pub fn plugin_record_not_found(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::PluginRecordNotFound, detail)
    }

    /// `PluginNameConflict` (`409`).
    #[must_use]
    pub fn plugin_name_conflict(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::PluginNameConflict, detail)
    }

    /// `PluginInUse` (`409`).
    #[must_use]
    pub fn plugin_in_use(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::PluginInUse, detail)
    }

    /// `LinkUnavailable` (`503`) — the upstream-disabled outcome
    /// (`cpt-cf-oagw-dod-upstream-disabled-outcome`).
    #[must_use]
    pub fn link_unavailable(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::LinkUnavailable, detail)
    }

    /// `MissingTargetHost` (`400`) — a common-suffix-alias, multi-endpoint
    /// upstream received no `X-OAGW-Target-Host` header
    /// (`cpt-cf-oagw-dod-target-host-selection`).
    #[must_use]
    pub fn missing_target_host(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::MissingTargetHost, detail)
    }

    /// `InvalidTargetHost` (`400`) — the `X-OAGW-Target-Host` value is not a
    /// bare hostname or IP address (`cpt-cf-oagw-dod-target-host-selection`).
    #[must_use]
    pub fn invalid_target_host(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::InvalidTargetHost, detail)
    }

    /// `UnknownTargetHost` (`400`) — the `X-OAGW-Target-Host` value matches
    /// no configured endpoint host (`cpt-cf-oagw-dod-target-host-selection`).
    #[must_use]
    pub fn unknown_target_host(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::UnknownTargetHost, detail)
    }

    /// `ProtocolError` (`502`) — a non-`101` upgrade handshake, or an
    /// endpoint scheme that cannot serve the requested transport
    /// (`cpt-cf-oagw-algo-upstream-invocation`).
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::ProtocolError, detail)
    }

    /// `CorsOriginNotAllowed` (`403`) (`cpt-cf-oagw-dod-cors-request-enforcement`).
    #[must_use]
    pub fn cors_origin_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::CorsOriginNotAllowed, detail)
    }

    /// `CorsMethodNotAllowed` (`403`) (`cpt-cf-oagw-dod-cors-request-enforcement`).
    #[must_use]
    pub fn cors_method_not_allowed(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::CorsMethodNotAllowed, detail)
    }

    /// `PluginNotFound` (`503`) — an unresolvable plugin reference, or a
    /// structurally invalid binding configuration (an inline secret value
    /// where a reference is required, or an oauth2 binding naming both a
    /// token endpoint and an issuer) (`cpt-cf-oagw-dod-runtime-plugin-resolution`,
    /// `cpt-cf-oagw-dod-credential-isolation`).
    #[must_use]
    pub fn plugin_not_found(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::PluginNotFound, detail)
    }

    /// `StreamAborted` (`502`) — an SSE upstream stream aborted after the
    /// response head arrived but before any body byte reached the caller
    /// (`cpt-cf-oagw-dod-sse-abort-handling`, CONS-F-001).
    #[must_use]
    pub fn stream_aborted(detail: impl Into<String>) -> Self {
        Self::new(OagwErrorKind::StreamAborted, detail)
    }

    /// This error's documented type.
    #[must_use]
    pub const fn kind(&self) -> OagwErrorKind {
        self.kind
    }

    /// The HTTP status this error maps to.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.kind.status()
    }

    /// Sets the RFC 9457 `instance` field: a URI reference identifying the
    /// specific occurrence, typically the request path.
    #[must_use]
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Attaches the `upstream_id` extension field.
    #[must_use]
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    /// Attaches the `host` extension field.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Attaches the `path` extension field.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attaches the `retry_after_seconds` extension field.
    #[must_use]
    pub fn with_retry_after_seconds(mut self, retry_after_seconds: u64) -> Self {
        self.retry_after_seconds = Some(retry_after_seconds);
        self
    }

    /// Attaches the `trace_id` extension field.
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Attaches the `plugin_id` extension field
    /// (`cpt-cf-oagw-dod-plugin-delete`).
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_id = Some(plugin_id.into());
        self
    }

    /// Attaches the `referenced_by.upstreams`/`referenced_by.routes`
    /// extension fields (`cpt-cf-oagw-dod-plugin-delete`,
    /// `cpt-cf-oagw-adr-request-routing`).
    #[must_use]
    pub fn with_referenced_by(mut self, upstreams: Vec<String>, routes: Vec<String>) -> Self {
        self.referenced_upstreams = Some(upstreams);
        self.referenced_routes = Some(routes);
        self
    }

    /// Attaches the `invalid_value` extension field: the offending
    /// `X-OAGW-Target-Host` value (`cpt-cf-oagw-dod-target-host-selection`).
    #[must_use]
    pub fn with_invalid_value(mut self, invalid_value: impl Into<String>) -> Self {
        self.invalid_value = Some(invalid_value.into());
        self
    }

    /// Attaches the `valid_hosts` extension field: the endpoint hosts a
    /// `X-OAGW-Target-Host` value may legally name
    /// (`cpt-cf-oagw-dod-target-host-selection`).
    #[must_use]
    pub fn with_valid_hosts(mut self, valid_hosts: Vec<String>) -> Self {
        self.valid_hosts = Some(valid_hosts);
        self
    }

    /// Attaches the RFC 9457 `error_code` field, e.g. `REQUIRED_HEADER_MISSING`
    /// (`cpt-cf-oagw-dod-required-headers-guard`).
    #[must_use]
    pub fn with_error_code(mut self, error_code: impl Into<String>) -> Self {
        self.error_code = Some(error_code.into());
        self
    }

    /// Attaches one raw response header, applied in [`Self::into_response`]
    /// in addition to the problem-document body
    /// (`cpt-cf-oagw-dod-rate-limit-response`). Never used to carry a
    /// resolved credential, token, or secret reference value.
    #[must_use]
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.extra_headers.push((name, value));
        self
    }

    /// Builds the RFC 9457 problem document for this error
    /// (`cpt-cf-oagw-dod-problem-details-shape`): `type`, `title`, `status`,
    /// `detail`, and `instance` are always present, plus whichever
    /// extension fields (`upstream_id`, `host`, `path`,
    /// `retry_after_seconds`) were attached.
    #[must_use]
    pub fn to_problem(&self) -> Problem {
        let context = self.build_context();

        Problem {
            problem_type: self.kind.gts_type().to_owned(),
            title: self.kind.title().to_owned(),
            status: self.status().as_u16(),
            detail: self.detail.clone(),
            instance: Some(
                self.instance
                    .clone()
                    .unwrap_or_else(|| UNSET_INSTANCE.to_owned()),
            ),
            trace_id: self.trace_id.clone(),
            context,
            error_code: self.error_code.clone(),
            error_domain: None,
        }
    }

    /// Assembles the RFC 9457 `context` extension object from every
    /// attached field, omitting any field never set
    /// (`cpt-cf-oagw-dod-problem-details-shape`).
    fn build_context(&self) -> Value {
        let mut context = Map::new();
        self.insert_simple_fields(&mut context);
        self.insert_target_host_fields(&mut context);
        self.insert_plugin_fields(&mut context);
        Value::Object(context)
    }

    /// Inserts the plain scalar extension fields shared by most error kinds.
    fn insert_simple_fields(&self, context: &mut Map<String, Value>) {
        if let Some(upstream_id) = &self.upstream_id {
            context.insert("upstream_id".to_owned(), Value::String(upstream_id.clone()));
        }
        if let Some(host) = &self.host {
            context.insert("host".to_owned(), Value::String(host.clone()));
        }
        if let Some(path) = &self.path {
            context.insert("path".to_owned(), Value::String(path.clone()));
        }
        if let Some(retry_after_seconds) = self.retry_after_seconds {
            context.insert(
                "retry_after_seconds".to_owned(),
                Value::from(retry_after_seconds),
            );
        }
    }

    /// Inserts the `X-OAGW-Target-Host` error extension fields
    /// (`cpt-cf-oagw-dod-target-host-selection`).
    fn insert_target_host_fields(&self, context: &mut Map<String, Value>) {
        if let Some(invalid_value) = &self.invalid_value {
            context.insert(
                "invalid_value".to_owned(),
                Value::String(invalid_value.clone()),
            );
        }
        if let Some(valid_hosts) = &self.valid_hosts {
            context.insert("valid_hosts".to_owned(), Value::from(valid_hosts.clone()));
        }
    }

    /// Inserts the plugin-management extension fields
    /// (`cpt-cf-oagw-dod-plugin-delete`).
    fn insert_plugin_fields(&self, context: &mut Map<String, Value>) {
        if let Some(plugin_id) = &self.plugin_id {
            context.insert("plugin_id".to_owned(), Value::String(plugin_id.clone()));
        }
        if self.referenced_upstreams.is_some() || self.referenced_routes.is_some() {
            let upstreams = self.referenced_upstreams.clone().unwrap_or_default();
            let routes = self.referenced_routes.clone().unwrap_or_default();
            context.insert(
                "referenced_by".to_owned(),
                serde_json::json!({ "upstreams": upstreams, "routes": routes }),
            );
        }
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        let extra_headers = self.extra_headers.clone();
        let mut response = self.to_problem().into_response();
        stamp_gateway_source(&mut response);
        for (name, value) in extra_headers {
            response.headers_mut().insert(name, value);
        }
        response
    }
}

/// Stamps `X-OAGW-Error-Source: gateway` on a gateway-originated response
/// (`cpt-cf-oagw-dod-error-source-header`): validation, the mounted-router
/// fallback, or any other gear-level error handling.
pub fn stamp_gateway_source(response: &mut Response) {
    response
        .headers_mut()
        .insert(ERROR_SOURCE_HEADER.clone(), SOURCE_GATEWAY);
}

/// Stamps `X-OAGW-Error-Source: upstream` on a response that passes through
/// an upstream call unchanged (`cpt-cf-oagw-dod-error-source-header`).
/// Later data-plane features (`cpt-cf-oagw-feature-http-proxy` and
/// following) call this on every proxied response, success or failure.
pub fn stamp_upstream_source(response: &mut Response) {
    response
        .headers_mut()
        .insert(ERROR_SOURCE_HEADER.clone(), SOURCE_UPSTREAM);
}

/// Stamps `X-OAGW-Error-Source: gateway` only when no value is already
/// present (`cpt-cf-oagw-dod-error-source-header`'s "every response ...
/// success or failure" clause).
///
/// A successful management-endpoint response (e.g. `201 Created` from
/// `POST /oagw/v1/upstreams`) never flows through [`OagwError::into_response`]
/// or [`stamp_upstream_source`], so nothing stamps it otherwise; applied as
/// blanket middleware in `crate::api::rest::routes::register_routes`, this
/// fills that gap without overriding a `stamp_upstream_source` call made
/// earlier in the same response's construction (`cpt-cf-oagw-feature-http-proxy`,
/// `cpt-cf-oagw-feature-streaming-proxy`).
pub fn ensure_gateway_source(response: &mut Response) {
    if !response.headers().contains_key(&ERROR_SOURCE_HEADER) {
        stamp_gateway_source(response);
    }
}

#[cfg(test)]
mod tests {
    use super::{OagwError, OagwErrorKind, stamp_gateway_source, stamp_upstream_source};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;

    const ALL_KINDS_WITH_STATUS: &[(OagwErrorKind, StatusCode)] = &[
        (OagwErrorKind::ValidationError, StatusCode::BAD_REQUEST),
        (
            OagwErrorKind::AuthenticationFailed,
            StatusCode::UNAUTHORIZED,
        ),
        (OagwErrorKind::RouteNotFound, StatusCode::NOT_FOUND),
        (OagwErrorKind::UpstreamNotFound, StatusCode::NOT_FOUND),
        (OagwErrorKind::AliasConflict, StatusCode::CONFLICT),
        (OagwErrorKind::RouteRecordNotFound, StatusCode::NOT_FOUND),
        (OagwErrorKind::RouteConflict, StatusCode::CONFLICT),
        (
            OagwErrorKind::PayloadTooLarge,
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        (
            OagwErrorKind::RateLimitExceeded,
            StatusCode::TOO_MANY_REQUESTS,
        ),
        (
            OagwErrorKind::SecretNotFound,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        (OagwErrorKind::DownstreamError, StatusCode::BAD_GATEWAY),
        (
            OagwErrorKind::CircuitBreakerOpen,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            OagwErrorKind::ConnectionTimeout,
            StatusCode::GATEWAY_TIMEOUT,
        ),
        (OagwErrorKind::RequestTimeout, StatusCode::GATEWAY_TIMEOUT),
        (OagwErrorKind::IdleTimeout, StatusCode::GATEWAY_TIMEOUT),
        (OagwErrorKind::PluginRecordNotFound, StatusCode::NOT_FOUND),
        (OagwErrorKind::PluginNameConflict, StatusCode::CONFLICT),
        (OagwErrorKind::PluginInUse, StatusCode::CONFLICT),
        (
            OagwErrorKind::LinkUnavailable,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (OagwErrorKind::MissingTargetHost, StatusCode::BAD_REQUEST),
        (OagwErrorKind::InvalidTargetHost, StatusCode::BAD_REQUEST),
        (OagwErrorKind::UnknownTargetHost, StatusCode::BAD_REQUEST),
        (OagwErrorKind::ProtocolError, StatusCode::BAD_GATEWAY),
        (OagwErrorKind::CorsOriginNotAllowed, StatusCode::FORBIDDEN),
        (OagwErrorKind::CorsMethodNotAllowed, StatusCode::FORBIDDEN),
        (
            OagwErrorKind::PluginNotFound,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (OagwErrorKind::StreamAborted, StatusCode::BAD_GATEWAY),
    ];

    // @cpt-begin:cpt-cf-oagw-dod-error-status-mapping:p1:inst-err-map-status-test-01
    #[test]
    fn every_documented_error_type_maps_to_its_fixed_status() {
        for (kind, expected_status) in ALL_KINDS_WITH_STATUS {
            assert_eq!(
                kind.status(),
                *expected_status,
                "{kind:?} must map to {expected_status}"
            );
        }
    }
    // @cpt-end:cpt-cf-oagw-dod-error-status-mapping:p1:inst-err-map-status-test-01

    #[test]
    fn timeouts_are_three_distinct_types_all_mapped_to_504() {
        assert_eq!(
            OagwErrorKind::ConnectionTimeout.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            OagwErrorKind::RequestTimeout.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            OagwErrorKind::IdleTimeout.status(),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_ne!(
            OagwErrorKind::ConnectionTimeout,
            OagwErrorKind::RequestTimeout
        );
        assert_ne!(OagwErrorKind::RequestTimeout, OagwErrorKind::IdleTimeout);
    }

    // @cpt-begin:cpt-cf-oagw-dod-problem-details-shape:p1:inst-err-map-shape-test-01
    #[test]
    fn route_not_found_gts_type_matches_the_documented_identifier() {
        let problem = OagwError::route_not_found("no route matches /oagw/v1/whatever")
            .with_instance("/oagw/v1/whatever")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(problem.title, "Route Not Found");
        assert_eq!(problem.status, 404);
        assert!(!problem.detail.is_empty());
        assert_eq!(problem.instance.as_deref(), Some("/oagw/v1/whatever"));
    }

    // @cpt-begin:cpt-cf-oagw-dod-alias-uniqueness:p1:inst-err-map-alias-conflict-test-01
    #[test]
    fn alias_conflict_maps_to_409_with_a_dedicated_gts_type() {
        let problem = OagwError::alias_conflict("alias 'vendor.com' already exists")
            .with_instance("/oagw/v1/upstreams")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1"
        );
        assert_eq!(problem.status, 409);
    }
    // @cpt-end:cpt-cf-oagw-dod-alias-uniqueness:p1:inst-err-map-alias-conflict-test-01

    // @cpt-begin:cpt-cf-oagw-dod-route-tenant-scoping:p1:inst-err-map-route-404-test-01
    #[test]
    fn route_record_not_found_maps_to_404_with_a_dedicated_gts_type() {
        let problem = OagwError::route_record_not_found("no such route")
            .with_instance("/oagw/v1/routes/00000000-0000-0000-0000-000000000000")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.route.record_not_found.v1"
        );
        assert_eq!(problem.status, 404);
    }
    // @cpt-end:cpt-cf-oagw-dod-route-tenant-scoping:p1:inst-err-map-route-404-test-01

    // @cpt-begin:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-err-map-route-conflict-test-01
    #[test]
    fn route_conflict_maps_to_409_with_a_dedicated_gts_type() {
        let problem = OagwError::route_conflict("duplicate path/priority/method")
            .with_instance("/oagw/v1/routes")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.route.conflict.v1"
        );
        assert_eq!(problem.status, 409);
    }
    // @cpt-end:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-err-map-route-conflict-test-01

    // @cpt-begin:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-err-map-upstream-404-test-01
    #[test]
    fn upstream_not_found_maps_to_404_with_a_dedicated_gts_type() {
        let problem = OagwError::upstream_not_found("no such upstream")
            .with_instance("/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1"
        );
        assert_eq!(problem.status, 404);
    }
    // @cpt-end:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-err-map-upstream-404-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-identification:p1:inst-err-map-plugin-404-test-01
    #[test]
    fn plugin_record_not_found_maps_to_404_with_a_dedicated_gts_type_distinct_from_plugin_not_found()
     {
        let problem = OagwError::plugin_record_not_found("no such plugin")
            .with_instance("/oagw/v1/plugins/gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000000")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.record_not_found.v1"
        );
        assert_eq!(problem.status, 404);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-identification:p1:inst-err-map-plugin-404-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-err-map-plugin-name-conflict-test-01
    #[test]
    fn plugin_name_conflict_maps_to_409_with_a_dedicated_gts_type() {
        let problem = OagwError::plugin_name_conflict("plugin 'my_guard' already exists")
            .with_instance("/oagw/v1/plugins")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.name_conflict.v1"
        );
        assert_eq!(problem.status, 409);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-err-map-plugin-name-conflict-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-err-map-plugin-in-use-test-01
    #[test]
    fn plugin_in_use_maps_to_409_and_carries_plugin_id_and_referenced_by() {
        let problem = OagwError::plugin_in_use("plugin is referenced by 1 upstream(s) and 0 route(s)")
            .with_instance("/oagw/v1/plugins/gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000000")
            .with_plugin_id("gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000000")
            .with_referenced_by(
                vec!["gts.cf.core.oagw.upstream.v1~11111111-1111-1111-1111-111111111111".to_owned()],
                Vec::new(),
            )
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
        );
        assert_eq!(problem.status, 409);
        assert_eq!(
            problem.context["plugin_id"],
            "gts.cf.core.oagw.guard_plugin.v1~00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            problem.context["referenced_by"]["upstreams"],
            serde_json::json!([
                "gts.cf.core.oagw.upstream.v1~11111111-1111-1111-1111-111111111111"
            ])
        );
        assert_eq!(
            problem.context["referenced_by"]["routes"],
            serde_json::json!([])
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-err-map-plugin-in-use-test-01

    // @cpt-begin:cpt-cf-oagw-dod-upstream-disabled-outcome:p1:inst-err-map-link-unavailable-test-01
    #[test]
    fn link_unavailable_maps_to_503_with_the_documented_gts_type() {
        let problem = OagwError::link_unavailable("alias 'api.example.com' is disabled")
            .with_instance("/oagw/v1/proxy/api.example.com/v1")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
        assert_eq!(problem.title, "Link Unavailable");
        assert_eq!(problem.status, 503);
    }
    // @cpt-end:cpt-cf-oagw-dod-upstream-disabled-outcome:p1:inst-err-map-link-unavailable-test-01

    #[test]
    fn problem_document_always_carries_non_empty_required_fields() {
        for (kind, _) in ALL_KINDS_WITH_STATUS {
            let problem = OagwError::new(*kind, "some detail").to_problem();
            assert!(!problem.problem_type.is_empty());
            assert!(!problem.title.is_empty());
            assert!(problem.status > 0);
            assert!(!problem.detail.is_empty());
            assert!(
                problem
                    .instance
                    .is_some_and(|instance| !instance.is_empty())
            );
        }
    }

    #[test]
    fn extension_fields_are_attached_only_when_present() {
        let bare = OagwError::downstream_error("boom").to_problem();
        assert_eq!(bare.context, serde_json::json!({}));

        let decorated = OagwError::downstream_error("boom")
            .with_upstream_id("upstream-1")
            .with_host("api.example.com")
            .with_path("/v1/resource")
            .with_retry_after_seconds(15)
            .to_problem();
        assert_eq!(
            decorated.context,
            serde_json::json!({
                "upstream_id": "upstream-1",
                "host": "api.example.com",
                "path": "/v1/resource",
                "retry_after_seconds": 15,
            })
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-problem-details-shape:p1:inst-err-map-shape-test-01

    // @cpt-begin:cpt-cf-oagw-dod-error-source-header:p1:inst-src-stamp-test-01
    #[test]
    fn into_response_renders_problem_json_and_stamps_gateway_source() {
        let response = OagwError::rate_limit_exceeded("too many requests")
            .with_instance("/oagw/v1/proxy/acme")
            .with_retry_after_seconds(15)
            .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some(APPLICATION_PROBLEM_JSON)
        );
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }

    #[test]
    fn stamp_upstream_source_sets_the_upstream_value() {
        let mut response = axum::http::StatusCode::OK.into_response();
        stamp_upstream_source(&mut response);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("upstream")
        );
    }

    // @cpt-begin:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-err-map-plugin-not-found-test-01
    #[test]
    fn plugin_not_found_maps_to_503_and_is_distinct_from_plugin_record_not_found() {
        let problem = OagwError::plugin_not_found("no backing implementation")
            .with_instance("/oagw/v1/proxy/api.example.com/v1")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
        );
        assert_eq!(problem.status, 503);
        assert_ne!(
            OagwErrorKind::PluginNotFound,
            OagwErrorKind::PluginRecordNotFound
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-err-map-plugin-not-found-test-01

    // @cpt-begin:cpt-cf-oagw-dod-required-headers-guard:p2:inst-err-map-error-code-test-01
    #[test]
    fn with_error_code_surfaces_on_the_rendered_problem_document() {
        let problem = OagwError::validation_error("missing header")
            .with_error_code("REQUIRED_HEADER_MISSING")
            .to_problem();
        assert_eq!(
            problem.error_code.as_deref(),
            Some("REQUIRED_HEADER_MISSING")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-required-headers-guard:p2:inst-err-map-error-code-test-01

    // @cpt-begin:cpt-cf-oagw-dod-rate-limit-response:p2:inst-err-map-extra-headers-test-01
    #[test]
    fn with_header_attaches_a_raw_response_header() {
        let response = OagwError::rate_limit_exceeded("too many requests")
            .with_instance("/oagw/v1/proxy/acme")
            .with_header(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static("3"),
            )
            .into_response();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("3")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-rate-limit-response:p2:inst-err-map-extra-headers-test-01

    // CONS-F-001 regression: `StreamAborted` must map to `502` with the
    // exact GTS type `DESIGN.md` documents.
    #[test]
    fn stream_aborted_maps_to_502_with_the_documented_gts_type() {
        let problem = OagwError::stream_aborted("upstream sse stream aborted before any byte")
            .with_instance("/oagw/v1/proxy/api.example.com/v1/stream")
            .to_problem();

        assert_eq!(
            problem.problem_type,
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
        );
        assert_eq!(problem.title, "Stream Aborted");
        assert_eq!(problem.status, 502);
    }

    #[test]
    fn stamp_gateway_source_overrides_any_prior_value() {
        let mut response = axum::http::StatusCode::OK.into_response();
        stamp_upstream_source(&mut response);
        stamp_gateway_source(&mut response);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-error-source-header:p1:inst-src-stamp-test-01
}
