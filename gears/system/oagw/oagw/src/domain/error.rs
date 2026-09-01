//! Domain error model.
//!
//! Every variant maps to one row of the OAGW error catalog (DESIGN §3.3
//! "Error Response Format" / ADR-0007): a status code, a GTS problem `type`
//! identifier and a title. The transport layer renders these into
//! `application/problem+json` bodies with `X-OAGW-Error-Source: gateway`.

use std::time::Duration;

/// GTS problem `type` identifiers from the OAGW error catalog.
///
/// Management-plane codes outside the documented catalog follow the same
/// `gts.cf.core.errors.err.v1~cf.oagw.<resource>.<event>.v1` family.
pub mod codes {
    /// 400 — general route/request validation error.
    pub const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    /// 400 — `X-OAGW-Target-Host` required but absent.
    pub const MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    /// 400 — `X-OAGW-Target-Host` malformed.
    pub const INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    /// 400 — `X-OAGW-Target-Host` does not match a configured endpoint.
    pub const UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    /// 401 — upstream authentication failed.
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    /// 404 — no route matched the request.
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    /// 404 — management resource absent.
    pub const RESOURCE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.resource.not_found.v1";
    /// 409 — alias already bound in the tenant scope.
    pub const ALIAS_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.upstream.alias_conflict.v1";
    /// 409 — plugin still referenced by an upstream or route.
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    /// 409 — concurrent modification of the same resource.
    pub const CONCURRENT_MODIFICATION: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.resource.conflict.v1";
    /// 413 — request body above the hard limit.
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    /// 429 — rate limit exceeded.
    pub const RATE_LIMIT_EXCEEDED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    /// 500 — referenced secret absent from the credential store.
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    /// 502 — protocol-level failure with the upstream.
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    /// 502 — upstream returned a non-proxied error (DownstreamError row).
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    /// 502 — streaming relay aborted.
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    /// 503 — upstream link unavailable.
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    /// 503 — circuit breaker open.
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    /// 503 — referenced plugin unresolvable.
    pub const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    /// 504 — upstream connection timeout.
    pub const CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    /// 504 — upstream request timeout.
    pub const REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    /// 504 — stream idle timeout.
    pub const IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
    /// 403 — CORS origin rejected (ADR-0004).
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    /// 403 — CORS method rejected (ADR-0004).
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
    /// 400 — route match rejected a request (method/query/suffix rule).
    pub const ROUTE_REJECTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.rejected.v1";
}

/// Alias for domain fallibility.
pub type DomainResult<T> = Result<T, DomainError>;

/// Failure modes of the OAGW domain logic.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    /// Request or payload failed validation (400).
    #[error("validation failed: {0}")]
    Validation(String),
    /// General route validation error (400).
    #[error("route error: {0}")]
    RouteError(String),
    /// A route rule rejected the request (method/query/path-suffix).
    #[error("request rejected by route: {0}")]
    RouteRejected(String),
    /// `X-OAGW-Target-Host` required but absent (400).
    #[error("X-OAGW-Target-Host header required to disambiguate upstream endpoints")]
    MissingTargetHost { alias: String },
    /// `X-OAGW-Target-Host` malformed (400).
    #[error("X-OAGW-Target-Host header is invalid: {value}")]
    InvalidTargetHost { value: String },
    /// `X-OAGW-Target-Host` does not match a configured endpoint (400).
    #[error("X-OAGW-Target-Host does not match any configured endpoint: {value}")]
    UnknownTargetHost { value: String },
    /// Upstream authentication failed (401).
    #[error("authentication to upstream failed: {0}")]
    AuthenticationFailed(String),
    /// The caller is not authenticated (401).
    #[error("{0}")]
    Unauthorized(String),
    /// No route matched (404).
    #[error("no matching route found: {0}")]
    RouteNotFound(String),
    /// Management resource absent (404).
    #[error("{resource} not found: {id}")]
    NotFound { resource: &'static str, id: String },
    /// Alias already bound in the tenant scope (409).
    #[error("upstream alias already in use: {alias}")]
    AliasConflict { alias: String },
    /// Route match rule duplicates an existing route (409).
    #[error("route match conflicts with an existing route: {0}")]
    RouteConflict(String),
    /// Plugin still referenced (409).
    #[error("plugin is referenced by other resources: {plugin_id}")]
    PluginInUse {
        plugin_id: String,
        upstreams: Vec<String>,
        routes: Vec<String>,
    },
    /// Concurrent modification (409).
    #[error("resource was modified concurrently: {0}")]
    ConcurrentModification(String),
    /// Request body above the hard limit (413).
    #[error("request payload exceeds the configured limit of {limit} bytes")]
    PayloadTooLarge { limit: u64 },
    /// Rate limit exceeded (429).
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        limit: u64,
        remaining: u64,
        reset_seconds: u64,
        retry_after_seconds: u64,
    },
    /// CORS origin rejected (403, ADR-0004).
    #[error("origin is not allowed by the CORS configuration: {origin}")]
    CorsOriginNotAllowed { origin: String },
    /// CORS method rejected (403, ADR-0004).
    #[error("method is not allowed by the CORS configuration: {method}")]
    CorsMethodNotAllowed { method: String },
    /// Referenced secret absent (500).
    #[error("referenced secret not found: {0}")]
    SecretNotFound(String),
    /// Protocol-level upstream failure (502).
    #[error("protocol error communicating with the upstream: {0}")]
    ProtocolError(String),
    /// The upstream returned a non-proxied error (502, DownstreamError row).
    #[error("upstream service error: {0}")]
    DownstreamError(String),
    /// Streaming relay aborted (502).
    #[error("stream aborted: {0}")]
    StreamAborted(String),
    /// The upstream declined a WebSocket upgrade with its own status (502).
    #[error("upstream refused the WebSocket upgrade with status {status}")]
    UpstreamRejectedUpgrade {
        /// Status the upstream answered the handshake with.
        status: u16,
    },
    /// Upstream link unavailable (503).
    #[error("upstream link unavailable: {0}")]
    LinkUnavailable(String),
    /// Circuit breaker open (503).
    #[error("circuit breaker open for {0}")]
    CircuitBreakerOpen(String),
    /// Referenced plugin unresolvable (503).
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// Upstream connect timeout (504).
    #[error("connection to the upstream timed out")]
    ConnectionTimeout,
    /// Upstream request timeout (504).
    #[error("upstream request timed out")]
    RequestTimeout,
    /// Stream idle timeout (504).
    #[error("stream idle timeout")]
    IdleTimeout,
    /// Unhandled internal failure (500).
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// HTTP status code for this error (DESIGN §3.3 error catalog).
    #[must_use]
    pub const fn status_code(&self) -> u16 {
        match self {
            Self::Validation(_)
            | Self::RouteError(_)
            | Self::RouteRejected(_)
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => 400,
            Self::AuthenticationFailed(_) | Self::Unauthorized(_) => 401,
            Self::CorsOriginNotAllowed { .. } | Self::CorsMethodNotAllowed { .. } => 403,
            Self::RouteNotFound(_) | Self::NotFound { .. } => 404,
            Self::AliasConflict { .. }
            | Self::RouteConflict(_)
            | Self::PluginInUse { .. }
            | Self::ConcurrentModification(_) => 409,
            Self::PayloadTooLarge { .. } => 413,
            Self::RateLimitExceeded { .. } => 429,
            Self::SecretNotFound(_) | Self::Internal(_) => 500,
            Self::ProtocolError(_)
            | Self::StreamAborted(_)
            | Self::DownstreamError(_)
            | Self::UpstreamRejectedUpgrade { .. } => 502,
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen(_) | Self::PluginNotFound(_) => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    /// GTS problem `type` identifier for this error.
    #[must_use]
    pub const fn gts_type(&self) -> &'static str {
        match self {
            Self::Validation(_) | Self::RouteError(_) => codes::VALIDATION,
            Self::RouteRejected(_) => codes::ROUTE_REJECTED,
            Self::MissingTargetHost { .. } => codes::MISSING_TARGET_HOST,
            Self::InvalidTargetHost { .. } => codes::INVALID_TARGET_HOST,
            Self::UnknownTargetHost { .. } => codes::UNKNOWN_TARGET_HOST,
            Self::AuthenticationFailed(_) | Self::Unauthorized(_) => codes::AUTH_FAILED,
            Self::RouteNotFound(_) => codes::ROUTE_NOT_FOUND,
            Self::NotFound { .. } => codes::RESOURCE_NOT_FOUND,
            Self::AliasConflict { .. } => codes::ALIAS_CONFLICT,
            Self::RouteConflict(_) | Self::ConcurrentModification(_) => {
                codes::CONCURRENT_MODIFICATION
            }
            Self::PluginInUse { .. } => codes::PLUGIN_IN_USE,
            Self::PayloadTooLarge { .. } => codes::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => codes::RATE_LIMIT_EXCEEDED,
            Self::CorsOriginNotAllowed { .. } => codes::CORS_ORIGIN_NOT_ALLOWED,
            Self::CorsMethodNotAllowed { .. } => codes::CORS_METHOD_NOT_ALLOWED,
            Self::SecretNotFound(_) => codes::SECRET_NOT_FOUND,
            Self::ProtocolError(_) => codes::PROTOCOL_ERROR,
            Self::DownstreamError(_) => codes::DOWNSTREAM_ERROR,
            Self::StreamAborted(_) => codes::STREAM_ABORTED,
            Self::UpstreamRejectedUpgrade { .. } => codes::PROTOCOL_ERROR,
            Self::LinkUnavailable(_) => codes::LINK_UNAVAILABLE,
            Self::CircuitBreakerOpen(_) => codes::CIRCUIT_BREAKER_OPEN,
            Self::PluginNotFound(_) => codes::PLUGIN_NOT_FOUND,
            Self::ConnectionTimeout => codes::CONNECTION_TIMEOUT,
            Self::RequestTimeout => codes::REQUEST_TIMEOUT,
            Self::IdleTimeout => codes::IDLE_TIMEOUT,
            Self::Internal(_) => "gts.cf.core.errors.err.v1~cf.core.err.internal.v1",
        }
    }

    /// Stable machine-readable error type used by metrics and audit logs.
    #[must_use]
    pub const fn error_type(&self) -> &'static str {
        match self {
            Self::Validation(_) => "ValidationError",
            Self::RouteError(_) => "RouteError",
            Self::RouteRejected(_) => "RouteRejected",
            Self::MissingTargetHost { .. } => "MissingTargetHost",
            Self::InvalidTargetHost { .. } => "InvalidTargetHost",
            Self::UnknownTargetHost { .. } => "UnknownTargetHost",
            Self::AuthenticationFailed(_) => "AuthenticationFailed",
            Self::Unauthorized(_) => "Unauthorized",
            Self::RouteNotFound(_) => "RouteNotFound",
            Self::NotFound { .. } => "NotFound",
            Self::AliasConflict { .. } => "AliasConflict",
            Self::RouteConflict(_) => "RouteConflict",
            Self::PluginInUse { .. } => "PluginInUse",
            Self::ConcurrentModification(_) => "ConcurrentModification",
            Self::PayloadTooLarge { .. } => "PayloadTooLarge",
            Self::RateLimitExceeded { .. } => "RateLimitExceeded",
            Self::CorsOriginNotAllowed { .. } => "CorsOriginNotAllowed",
            Self::CorsMethodNotAllowed { .. } => "CorsMethodNotAllowed",
            Self::SecretNotFound(_) => "SecretNotFound",
            Self::ProtocolError(_) => "ProtocolError",
            Self::DownstreamError(_) => "DownstreamError",
            Self::StreamAborted(_) => "StreamAborted",
            Self::UpstreamRejectedUpgrade { .. } => "UpstreamRejectedUpgrade",
            Self::LinkUnavailable(_) => "LinkUnavailable",
            Self::CircuitBreakerOpen(_) => "CircuitBreakerOpen",
            Self::PluginNotFound(_) => "PluginNotFound",
            Self::ConnectionTimeout => "ConnectionTimeout",
            Self::RequestTimeout => "RequestTimeout",
            Self::IdleTimeout => "IdleTimeout",
            Self::Internal(_) => "Internal",
        }
    }

    /// Retry guidance for retriable failures (ADR-0007 `retry_after_seconds`).
    #[must_use]
    pub const fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            Self::RateLimitExceeded {
                retry_after_seconds,
                ..
            } => Some(*retry_after_seconds),
            Self::LinkUnavailable(_)
            | Self::CircuitBreakerOpen(_)
            | Self::ConnectionTimeout
            | Self::RequestTimeout
            | Self::IdleTimeout => Some(1),
            _ => None,
        }
    }

    /// `true` when this failure was produced by the upstream rather than by the
    /// gateway, so the problem document is attributed `upstream` (ADR-0007).
    #[must_use]
    pub const fn is_upstream_failure(&self) -> bool {
        matches!(
            self,
            Self::DownstreamError(_) | Self::UpstreamRejectedUpgrade { .. }
        )
    }

    /// Build a validation error from a message.
    #[must_use]
    pub fn validation(message: impl Into<String>) -> Self {
        Self::Validation(message.into())
    }

    /// Build a not-found error for a named resource.
    #[must_use]
    pub fn not_found(resource: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource,
            id: id.into(),
        }
    }

    /// Build a rate-limit rejection.
    #[must_use]
    pub fn rate_limited(limit: u64, remaining: u64, reset_seconds: u64, retry_after: u64) -> Self {
        Self::RateLimitExceeded {
            limit,
            remaining,
            reset_seconds,
            retry_after_seconds: retry_after.max(1),
        }
    }
}

impl From<tokio::time::error::Elapsed> for DomainError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::RequestTimeout
    }
}

/// Helper: convert a connect/IO failure into the appropriate catalog entry.
#[must_use]
pub fn transport_error(error: &std::io::Error) -> DomainError {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
            DomainError::RequestTimeout
        }
        std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::UnexpectedEof
        | std::io::ErrorKind::NotFound
        | std::io::ErrorKind::PermissionDenied => DomainError::LinkUnavailable(error.to_string()),
        _ => DomainError::ProtocolError(error.to_string()),
    }
}

/// Helper: build a rate-limit reset delta from a token-bucket refill instant.
#[must_use]
pub fn retry_after_from(reset_in: Duration) -> u64 {
    whole_seconds(reset_in)
}

/// Convenience: `Duration` -> whole seconds for wire fields (at least one).
#[must_use]
pub fn whole_seconds(value: Duration) -> u64 {
    value.as_secs().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_statuses_match_the_catalog() {
        let cases: Vec<(DomainError, u16, &'static str)> = vec![
            (
                DomainError::MissingTargetHost {
                    alias: "vendor.com".to_owned(),
                },
                400,
                codes::MISSING_TARGET_HOST,
            ),
            (
                DomainError::RouteNotFound("no match".to_owned()),
                404,
                codes::ROUTE_NOT_FOUND,
            ),
            (
                DomainError::PluginInUse {
                    plugin_id: "p".to_owned(),
                    upstreams: vec![],
                    routes: vec![],
                },
                409,
                codes::PLUGIN_IN_USE,
            ),
            (
                DomainError::PayloadTooLarge { limit: 1 },
                413,
                codes::PAYLOAD_TOO_LARGE,
            ),
            (
                DomainError::rate_limited(10, 0, 3, 3),
                429,
                codes::RATE_LIMIT_EXCEEDED,
            ),
            (
                DomainError::ConnectionTimeout,
                504,
                codes::CONNECTION_TIMEOUT,
            ),
        ];
        for (error, status, gts) in cases {
            assert_eq!(error.status_code(), status, "{error:?}");
            assert_eq!(error.gts_type(), gts, "{error:?}");
        }
    }

    #[test]
    fn transport_failures_map_by_kind() {
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(
            transport_error(&refused),
            DomainError::LinkUnavailable(_)
        ));
        let timed_out = std::io::Error::from(std::io::ErrorKind::TimedOut);
        assert!(matches!(
            transport_error(&timed_out),
            DomainError::RequestTimeout
        ));
    }

    #[test]
    fn upstream_failures_are_attributed_to_the_upstream() {
        assert!(DomainError::DownstreamError("boom".to_owned()).is_upstream_failure());
        assert!(DomainError::UpstreamRejectedUpgrade { status: 404 }.is_upstream_failure());
        assert!(!DomainError::RouteNotFound("nope".to_owned()).is_upstream_failure());
    }

    #[test]
    fn retry_after_is_at_least_one_second() {
        assert_eq!(retry_after_from(Duration::from_millis(10)), 1);
        assert_eq!(whole_seconds(Duration::from_secs(4)), 4);
    }
}
