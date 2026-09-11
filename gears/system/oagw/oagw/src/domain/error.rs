//! Domain error model for the `oagw` gear.
//!
//! Every variant corresponds to one row of the error table in
//! `docs/DESIGN.md` §"Error semantics" (and `specs/.../contracts/errors.md`).
//! The HTTP mapping lives in [`crate::api::rest::error`]; this module only
//! carries the domain meaning and the occurrence-specific payload fields.

use std::collections::BTreeMap;

/// References held to a plugin, reported when a delete is refused.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReferencedBy {
    /// Upstream ids referencing the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    /// Route ids referencing the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
}

/// A `Retry-After` hint, in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter(pub u64);

/// Rate-limit snapshot carried by [`DomainError::RateLimitExceeded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitSnapshot {
    pub limit: u64,
    pub remaining: u64,
    /// Epoch seconds at which the bucket refills.
    pub reset: i64,
    pub retry_after: u64,
}

/// Coarse category of a [`DomainError`].
///
/// [`DomainError`] is `#[non_exhaustive]`, so callers that only need to know
/// *what kind* of failure occurred match on this instead of on the variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The request was invalid.
    Validation,
    /// The upstream refused the credentials.
    Authentication,
    /// The caller lacks permission.
    Permission,
    /// The addressed resource does not exist.
    NotFound,
    /// The request conflicts with existing state.
    Conflict,
    /// A rate limit was exhausted.
    RateLimit,
    /// A credential reference could not be resolved.
    Secret,
    /// The upstream failed.
    Upstream,
    /// The upstream is not currently reachable.
    Availability,
    /// The circuit breaker is open.
    CircuitBreaker,
    /// A plugin could not be resolved.
    Plugin,
    /// An operation timed out.
    Timeout,
    /// An unexpected internal failure.
    Internal,
}

/// Domain error for the `oagw` gear.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum DomainError {
    /// 400 — general validation failure.
    #[error("validation failed: {detail}")]
    ValidationError {
        detail: String,
    },

    /// 400 — multi-endpoint common-suffix upstream called without a target host.
    #[error("X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias")]
    MissingTargetHost {
        alias: String,
        valid_hosts: Vec<String>,
        upstream_id: String,
    },

    /// 400 — the target host header is not a hostname or IP.
    #[error("X-OAGW-Target-Host header format is invalid")]
    InvalidTargetHost {
        invalid_value: String,
    },

    /// 400 — the target host does not match any configured endpoint.
    #[error("X-OAGW-Target-Host header value does not match any configured endpoint")]
    UnknownTargetHost {
        invalid_value: String,
        valid_hosts: Vec<String>,
    },

    /// 401 — authentication to the upstream failed.
    #[error("authentication failed: {detail}")]
    AuthenticationFailed {
        detail: String,
        plugin_id: Option<String>,
    },

    /// 403 — the caller lacks the requested permission.
    #[error("permission denied: {detail}")]
    PermissionDenied {
        detail: String,
    },

    /// 404 — the addressed resource does not exist (or belongs to another tenant).
    #[error("resource not found: {detail}")]
    NotFound {
        detail: String,
    },

    /// 404 — no enabled upstream resolves for the requested alias, or no route matches.
    #[error("route not found: {detail}")]
    RouteNotFound {
        detail: String,
        alias: Option<String>,
        host: Option<String>,
        path: Option<String>,
    },

    /// 409 — the request conflicts with existing state.
    #[error("conflict: {detail}")]
    Conflict {
        detail: String,
    },

    /// 409 — a plugin delete was refused because it is still referenced.
    #[error("plugin is still referenced")]
    PluginInUse {
        plugin_id: String,
        referenced_by: ReferencedBy,
    },

    /// 413 — the request body exceeds the configured maximum.
    #[error("request payload exceeds the maximum allowed size")]
    PayloadTooLarge {
        detail: String,
    },

    /// 429 — a rate limit was exhausted.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        snapshot: RateLimitSnapshot,
        host: Option<String>,
        path: Option<String>,
        upstream_id: Option<String>,
    },

    /// 500 — a referenced credential could not be found in the credential store.
    #[error("referenced secret not found")]
    SecretNotFound {
        detail: String,
        plugin_id: Option<String>,
    },

    /// 502 — a protocol-level failure while talking to the upstream.
    #[error("protocol error: {detail}")]
    ProtocolError {
        detail: String,
        host: Option<String>,
    },

    /// 502 — the upstream returned an error the gear reports as a downstream failure.
    #[error("upstream service error (status {status})")]
    DownstreamError {
        status: u16,
        host: Option<String>,
    },

    /// 502 — a streamed response was aborted before completion.
    #[error("stream connection aborted")]
    StreamAborted {
        detail: String,
    },

    /// 503 — the upstream exists but is disabled.
    #[error("upstream link is unavailable (disabled)")]
    LinkUnavailable {
        detail: String,
        upstream_id: Option<String>,
        alias: Option<String>,
    },

    /// 503 — the circuit breaker for the host is open.
    #[error("circuit breaker is open")]
    CircuitBreakerOpen {
        host: Option<String>,
    },

    /// 503 — a referenced plugin could not be resolved.
    #[error("plugin not found")]
    PluginNotFound {
        plugin_id: String,
    },

    /// 503 — the gear cannot currently serve the request.
    #[error("service temporarily unavailable")]
    ServiceUnavailable {
        detail: String,
        retry_after: Option<u64>,
    },

    /// 504 — the connection to the upstream could not be established in time.
    #[error("connection to upstream timed out")]
    ConnectionTimeout {
        host: Option<String>,
    },

    /// 504 — the upstream did not answer the request in time.
    #[error("upstream request timed out")]
    RequestTimeout {
        host: Option<String>,
    },

    /// 504 — an established stream was idle for too long.
    #[error("upstream idle timeout")]
    IdleTimeout {
        host: Option<String>,
    },

    /// 500 — an unexpected internal failure.
    #[error("internal error")]
    Internal {
        diagnostic: String,
    },

    /// Extra, free-form context attached to an error occurrence.
    #[doc(hidden)]
    #[error("extra error context: {}", detail)]
    Extra {
        detail: String,
        fields: BTreeMap<String, String>,
    },
}

impl DomainError {
    /// Coarse category of the error, used for the `error_type` metric label
    /// and for callers that need to react without matching every variant.
    pub fn kind(&self) -> ErrorKind {
        use DomainError as E;
        match self {
            E::ValidationError { .. }
            | E::MissingTargetHost { .. }
            | E::InvalidTargetHost { .. }
            | E::UnknownTargetHost { .. }
            | E::Extra { .. } => ErrorKind::Validation,
            E::AuthenticationFailed { .. } => ErrorKind::Authentication,
            E::PermissionDenied { .. } => ErrorKind::Permission,
            E::NotFound { .. } | E::RouteNotFound { .. } => ErrorKind::NotFound,
            E::Conflict { .. } | E::PluginInUse { .. } => ErrorKind::Conflict,
            E::PayloadTooLarge { .. } => ErrorKind::Validation,
            E::RateLimitExceeded { .. } => ErrorKind::RateLimit,
            E::SecretNotFound { .. } => ErrorKind::Secret,
            E::ProtocolError { .. }
            | E::DownstreamError { .. }
            | E::StreamAborted { .. } => ErrorKind::Upstream,
            E::LinkUnavailable { .. } | E::ServiceUnavailable { .. } => ErrorKind::Availability,
            E::CircuitBreakerOpen { .. } => ErrorKind::CircuitBreaker,
            E::PluginNotFound { .. } => ErrorKind::Plugin,
            E::ConnectionTimeout { .. }
            | E::RequestTimeout { .. }
            | E::IdleTimeout { .. } => ErrorKind::Timeout,
            E::Internal { .. } => ErrorKind::Internal,
        }
    }

    /// Convenience constructor for [`DomainError::ValidationError`].
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::ValidationError { detail: detail.into() }
    }

    /// Convenience constructor for [`DomainError::NotFound`].
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::NotFound { detail: detail.into() }
    }

    /// Convenience constructor for [`DomainError::Conflict`].
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::Conflict { detail: detail.into() }
    }

    /// Convenience constructor for [`DomainError::PermissionDenied`].
    pub fn permission_denied(detail: impl Into<String>) -> Self {
        Self::PermissionDenied { detail: detail.into() }
    }

    /// Convenience constructor for [`DomainError::Internal`].
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal { diagnostic: diagnostic.into() }
    }

    /// The GTS error-type suffix documented for this variant
    /// (`gts.cf.core.errors.err.v1~cf.oagw.<suffix>`).
    pub fn error_type_suffix(&self) -> &'static str {
        use DomainError as E;
        match self {
            E::ValidationError { .. } | E::Extra { .. } => "validation.error.v1",
            E::MissingTargetHost { .. } => "routing.missing_target_host.v1",
            E::InvalidTargetHost { .. } => "routing.invalid_target_host.v1",
            E::UnknownTargetHost { .. } => "routing.unknown_target_host.v1",
            E::AuthenticationFailed { .. } => "auth.failed.v1",
            E::PermissionDenied { .. } => "permission.denied.v1",
            E::NotFound { .. } => "route.not_found.v1",
            E::RouteNotFound { .. } => "route.not_found.v1",
            E::Conflict { .. } => "conflict.v1",
            E::PluginInUse { .. } => "plugin.in_use.v1",
            E::PayloadTooLarge { .. } => "payload.too_large.v1",
            E::RateLimitExceeded { .. } => "rate_limit.exceeded.v1",
            E::SecretNotFound { .. } => "secret.not_found.v1",
            E::ProtocolError { .. } => "protocol.error.v1",
            E::DownstreamError { .. } => "downstream.error.v1",
            E::StreamAborted { .. } => "stream.aborted.v1",
            E::LinkUnavailable { .. } => "link.unavailable.v1",
            E::CircuitBreakerOpen { .. } => "circuit_breaker.open.v1",
            E::PluginNotFound { .. } => "plugin.not_found.v1",
            E::ConnectionTimeout { .. } => "timeout.connection.v1",
            E::RequestTimeout { .. } => "timeout.request.v1",
            E::IdleTimeout { .. } => "timeout.idle.v1",
            E::ServiceUnavailable { .. } => "service.unavailable.v1",
            E::Internal { .. } => "internal.v1",
        }
    }

    /// Human readable title for the variant (used as the problem `title`).
    pub fn title(&self) -> &'static str {
        use DomainError as E;
        match self {
            E::ValidationError { .. } | E::Extra { .. } => "Validation Error",
            E::MissingTargetHost { .. } => "Missing Target Host Header",
            E::InvalidTargetHost { .. } => "Invalid Target Host Format",
            E::UnknownTargetHost { .. } => "Unknown Target Host",
            E::AuthenticationFailed { .. } => "Authentication Failed",
            E::PermissionDenied { .. } => "Permission Denied",
            E::NotFound { .. } => "Not Found",
            E::RouteNotFound { .. } => "Route Not Found",
            E::Conflict { .. } => "Conflict",
            E::PluginInUse { .. } => "Plugin In Use",
            E::PayloadTooLarge { .. } => "Payload Too Large",
            E::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            E::SecretNotFound { .. } => "Secret Not Found",
            E::ProtocolError { .. } => "Protocol Error",
            E::DownstreamError { .. } => "Downstream Error",
            E::StreamAborted { .. } => "Stream Aborted",
            E::LinkUnavailable { .. } => "Link Unavailable",
            E::CircuitBreakerOpen { .. } => "Circuit Breaker Open",
            E::PluginNotFound { .. } => "Plugin Not Found",
            E::ConnectionTimeout { .. } => "Connection Timeout",
            E::RequestTimeout { .. } => "Request Timeout",
            E::IdleTimeout { .. } => "Idle Timeout",
            E::ServiceUnavailable { .. } => "Service Unavailable",
            E::Internal { .. } => "Internal Error",
        }
    }
}
