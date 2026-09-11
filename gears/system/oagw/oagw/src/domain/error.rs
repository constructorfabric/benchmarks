//! Canonical error type and mapping inputs.
//!
//! Realizes `cpt-cf-oagw-dod-error-catalogue`: one `ErrorKind` variant per
//! error type in the DESIGN §3.3 catalogue plus the two management-conflict
//! variants added per §1.5, each carrying its HTTP status, its GTS `type`
//! identifier, its Retriable flag, and its `gateway`/`upstream` source tag.
//!
//! The catalogue table is stated once, in the `match` arms below. The GTS
//! identifiers come verbatim from [`crate::gts`] — never synthesized from a
//! variant name. No `http`/`axum` type appears here; the RFC 9457 mapping
//! that consumes this type lives in [`crate::api`].

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::gts;

/// Why a domain model value failed its structural invariant.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    /// `server.endpoints` must carry at least one endpoint.
    #[error("server.endpoints requires at least one endpoint")]
    NoEndpoints,
    /// `route.match` must carry exactly one of `http` or `grpc`.
    #[error("route.match requires exactly one of http or grpc")]
    AmbiguousMatch,
}

/// Where a failure originated: the gateway itself, or the upstream service
/// it proxied to (`cpt-cf-oagw-adr-error-source-distinction`).
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorSource {
    /// The gateway answered the failure itself; mapped to
    /// `application/problem+json`.
    Gateway,
    /// The upstream service answered the failure; passed through with its own
    /// body and content type, never rewritten into a problem body.
    Upstream,
}

impl ErrorSource {
    /// The value of the `X-OAGW-Error-Source` header.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

impl fmt::Display for ErrorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Optional correlation and routing context attached to a [`DomainError`].
///
/// Every present member becomes an extension field of the problem body; no
/// member is ever defaulted.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorContext {
    /// Identifier of the upstream that failed.
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    /// Host the request was routed to.
    #[serde(default)]
    pub host: Option<String>,
    /// Request path.
    #[serde(default)]
    pub path: Option<String>,
    /// Suggested retry delay in seconds; emitted as `Retry-After` only for
    /// the six retriable rows.
    #[serde(default)]
    pub retry_after_seconds: Option<u64>,
    /// Correlation identifier, populated by the observability feature when a
    /// correlation context is available.
    #[serde(default)]
    pub trace_id: Option<String>,
}

// @cpt-dod:cpt-cf-oagw-dod-error-catalogue:p1
/// One variant per error type in the catalogue.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ErrorKind {
    /// Routing configuration error (400).
    RouteError,
    /// Validation error (400).
    ValidationError,
    /// The routed target has no host (400).
    MissingTargetHost,
    /// The routed target host is malformed (400).
    InvalidTargetHost,
    /// The routed target host is not configured (400).
    UnknownTargetHost,
    /// Authentication failed (401).
    AuthenticationFailed,
    /// No route matched (404).
    RouteNotFound,
    /// The plugin is referenced by a configuration (409).
    PluginInUse,
    /// The alias is already taken (409), added per §1.5.
    AliasConflict,
    /// The match rule conflicts with an existing route (409), added per §1.5.
    MatchConflict,
    /// The request body exceeds the configured ceiling (413).
    PayloadTooLarge,
    /// The rate limit is exhausted (429).
    RateLimitExceeded,
    /// The credential reference does not resolve (500).
    SecretNotFound,
    /// The upstream spoke a protocol the gateway cannot follow (502).
    ProtocolError,
    /// The upstream answered with a failure (502).
    DownstreamError,
    /// The stream was aborted (502).
    StreamAborted,
    /// No usable link to the upstream (503).
    LinkUnavailable,
    /// The circuit breaker is open (503).
    CircuitBreakerOpen,
    /// The referenced plugin is not resolvable (503).
    PluginNotFound,
    /// Establishing the connection timed out (504).
    ConnectionTimeout,
    /// The upstream did not answer in time (504).
    RequestTimeout,
    /// The idle deadline expired (504).
    IdleTimeout,
}

impl ErrorKind {
    /// Fixed human-readable problem `title` for the variant.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::RouteError => "Routing configuration error",
            Self::ValidationError => "Validation error",
            Self::MissingTargetHost => "Missing target host",
            Self::InvalidTargetHost => "Invalid target host",
            Self::UnknownTargetHost => "Unknown target host",
            Self::AuthenticationFailed => "Authentication failed",
            Self::RouteNotFound => "Route not found",
            Self::PluginInUse => "Plugin in use",
            Self::AliasConflict => "Alias conflict",
            Self::MatchConflict => "Match conflict",
            Self::PayloadTooLarge => "Payload too large",
            Self::RateLimitExceeded => "Rate limit exceeded",
            Self::SecretNotFound => "Secret not found",
            Self::ProtocolError => "Protocol error",
            Self::DownstreamError => "Downstream error",
            Self::StreamAborted => "Stream aborted",
            Self::LinkUnavailable => "Link unavailable",
            Self::CircuitBreakerOpen => "Circuit breaker open",
            Self::PluginNotFound => "Plugin not found",
            Self::ConnectionTimeout => "Connection timeout",
            Self::RequestTimeout => "Request timeout",
            Self::IdleTimeout => "Idle timeout",
        }
    }

    /// HTTP status of the catalogue row.
    #[must_use]
    pub const fn http_status(self) -> u16 {
        // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-type
        match self {
            Self::RouteError
            | Self::ValidationError
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthenticationFailed => 401,
            Self::RouteNotFound => 404,
            Self::PluginInUse | Self::AliasConflict | Self::MatchConflict => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::SecretNotFound => 500,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => 502,
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
        // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-type
    }

    /// Full GTS identifier of the catalogue row, `gts.cf.core.errors.err.v1~`
    /// prefix included, taken verbatim from the catalogue table.
    #[must_use]
    pub const fn gts_type(self) -> &'static str {
        match self {
            Self::RouteError | Self::ValidationError => gts::ERR_VALIDATION,
            Self::MissingTargetHost => gts::ERR_MISSING_TARGET_HOST,
            Self::InvalidTargetHost => gts::ERR_INVALID_TARGET_HOST,
            Self::UnknownTargetHost => gts::ERR_UNKNOWN_TARGET_HOST,
            Self::AuthenticationFailed => gts::ERR_AUTH_FAILED,
            Self::RouteNotFound => gts::ERR_ROUTE_NOT_FOUND,
            Self::PluginInUse => gts::ERR_PLUGIN_IN_USE,
            Self::AliasConflict => gts::ERR_ALIAS_CONFLICT,
            Self::MatchConflict => gts::ERR_MATCH_CONFLICT,
            Self::PayloadTooLarge => gts::ERR_PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded => gts::ERR_RATE_LIMIT_EXCEEDED,
            Self::SecretNotFound => gts::ERR_SECRET_NOT_FOUND,
            Self::ProtocolError => gts::ERR_PROTOCOL_ERROR,
            Self::DownstreamError => gts::ERR_DOWNSTREAM_ERROR,
            Self::StreamAborted => gts::ERR_STREAM_ABORTED,
            Self::LinkUnavailable => gts::ERR_LINK_UNAVAILABLE,
            Self::CircuitBreakerOpen => gts::ERR_CIRCUIT_BREAKER_OPEN,
            Self::PluginNotFound => gts::ERR_PLUGIN_NOT_FOUND,
            Self::ConnectionTimeout => gts::ERR_TIMEOUT_CONNECTION,
            Self::RequestTimeout => gts::ERR_TIMEOUT_REQUEST,
            Self::IdleTimeout => gts::ERR_TIMEOUT_IDLE,
        }
    }

    /// `true` for the six catalogue rows DESIGN §3.3 marks `Yes`:
    /// `RateLimitExceeded`, `LinkUnavailable`, `CircuitBreakerOpen`,
    /// `ConnectionTimeout`, `RequestTimeout`, `IdleTimeout`.
    ///
    /// `DownstreamError` is resolved non-retriable per §1.5: the retry
    /// decision for a 502 belongs to the caller and to the data-plane proxy,
    /// which owns upstream-failure policy.
    #[must_use]
    pub const fn is_retriable(self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded
                | Self::LinkUnavailable
                | Self::CircuitBreakerOpen
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.title())
    }
}

impl std::error::Error for ErrorKind {}

/// One gateway failure, carrying its catalogue row and its context.
///
/// The context is not duplicated per variant: `kind` selects the catalogue
/// row, and every variant shares the same `detail`, `source`, and `context`
/// shape.
///
/// `Display` and `std::error::Error` are implemented by hand rather than
/// derived: the mandated `source: ErrorSource` field would otherwise be
/// picked up by thiserror's automatic source detection and presented as the
/// error's cause chain.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainError {
    /// The catalogue row this failure answers with.
    pub kind: ErrorKind,
    /// Caller-supplied human-readable detail.
    ///
    /// Never contains credential material, a `cred://` reference value, or a
    /// configuration value; the mapper adds nothing to it.
    pub detail: String,
    /// Whether the gateway or the upstream produced the failure.
    pub source: ErrorSource,
    /// Optional correlation and routing context.
    pub context: ErrorContext,
}

impl DomainError {
    /// Builds a gateway-sourced failure with empty context.
    #[must_use]
    pub fn gateway(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: ErrorSource::Gateway,
            context: ErrorContext::default(),
        }
    }

    /// Builds an upstream-sourced failure with empty context.
    #[must_use]
    pub fn upstream(kind: ErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: ErrorSource::Upstream,
            context: ErrorContext::default(),
        }
    }

    /// Builds a gateway-sourced retriable failure whose `retry_after_seconds`
    /// the problem body and the `Retry-After` header carry, which is the
    /// context member the rate-limit and breaker answers are produced with.
    #[must_use]
    pub fn with_retry_after(
        kind: ErrorKind,
        detail: impl Into<String>,
        retry_after_seconds: Option<u64>,
    ) -> Self {
        Self {
            kind,
            detail: detail.into(),
            source: ErrorSource::Gateway,
            context: ErrorContext {
                retry_after_seconds,
                ..ErrorContext::default()
            },
        }
    }

    /// HTTP status of the catalogue row this failure answers with.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        self.kind.http_status()
    }

    /// Full GTS identifier of the catalogue row this failure answers with.
    #[must_use]
    pub const fn gts_type(&self) -> &'static str {
        self.kind.gts_type()
    }

    /// `true` for the six catalogue rows DESIGN §3.3 marks `Yes`.
    #[must_use]
    pub const fn is_retriable(&self) -> bool {
        self.kind.is_retriable()
    }

    /// `Retry-After` value in seconds: emitted only for the six retriable
    /// rows, and only when the context carries one.
    #[must_use]
    pub const fn retry_after_seconds(&self) -> Option<u64> {
        if self.kind.is_retriable() {
            self.context.retry_after_seconds
        } else {
            None
        }
    }
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.detail)
    }
}

impl std::error::Error for DomainError {}
