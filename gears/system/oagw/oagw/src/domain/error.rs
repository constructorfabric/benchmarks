//! Domain-level error types for OAGW.
//!
//! Every error carries the exact GTS instance identifier from DESIGN.md
//! §3.3 "Error Response Format".  Rendering to RFC 9457 problem+json with
//! `X-OAGW-Error-Source: gateway` happens in [`crate::api::rest::error`].

use thiserror::Error;

// ---- GTS instance identifiers (DESIGN.md error table, verbatim) --------

pub const GTS_VALIDATION_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
pub const GTS_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
pub const GTS_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
pub const GTS_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
pub const GTS_AUTHENTICATION_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
pub const GTS_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
pub const GTS_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
pub const GTS_PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
pub const GTS_RATE_LIMIT_EXCEEDED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
pub const GTS_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
pub const GTS_PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
pub const GTS_DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
pub const GTS_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
pub const GTS_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
pub const GTS_CIRCUIT_BREAKER_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
pub const GTS_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
pub const GTS_CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
pub const GTS_REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
pub const GTS_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
pub const GTS_CORS_ORIGIN_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
pub const GTS_CORS_METHOD_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// Conflict id for alias uniqueness violations (PRD: 409 Conflict).  Uses the
/// toolkit-canonical `AlreadyExists` identifier, re-scoped to OAGW.  The
/// DESIGN error table defines no dedicated alias-conflict instance id.
pub const GTS_ALIAS_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.core.err.already_exists.v1";

/// Id for internal/gateway-fault errors, whose documented status is 500 (in
/// contrast to the 502 `ProtocolError` family).  Mirrors the toolkit-canonical
/// `Internal` instance id used by the management-API mapping.
pub const GTS_INTERNAL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.core.err.internal.v1";

// ---- Permission identifiers (DESIGN.md §Permissions) -------------------

pub const PERM_UPSTREAM_CREATE: &str = "gts.cf.core.oagw.upstream.v1~:create";
pub const PERM_UPSTREAM_OVERRIDE: &str = "gts.cf.core.oagw.upstream.v1~:override";
pub const PERM_UPSTREAM_READ: &str = "gts.cf.core.oagw.upstream.v1~:read";
pub const PERM_UPSTREAM_DELETE: &str = "gts.cf.core.oagw.upstream.v1~:delete";
pub const PERM_UPSTREAM_BIND: &str = "gts.cf.core.oagw.upstream.v1~:bind";

pub const PERM_ROUTE_CREATE: &str = "gts.cf.core.oagw.route.v1~:create";
pub const PERM_ROUTE_OVERRIDE: &str = "gts.cf.core.oagw.route.v1~:override";
pub const PERM_ROUTE_READ: &str = "gts.cf.core.oagw.route.v1~:read";
pub const PERM_ROUTE_DELETE: &str = "gts.cf.core.oagw.route.v1~:delete";

pub const PERM_AUTH_PLUGIN_CREATE: &str = "gts.cf.core.oagw.auth_plugin.v1~:create";
pub const PERM_AUTH_PLUGIN_READ: &str = "gts.cf.core.oagw.auth_plugin.v1~:read";
pub const PERM_AUTH_PLUGIN_DELETE: &str = "gts.cf.core.oagw.auth_plugin.v1~:delete";

pub const PERM_GUARD_PLUGIN_CREATE: &str = "gts.cf.core.oagw.guard_plugin.v1~:create";
pub const PERM_GUARD_PLUGIN_READ: &str = "gts.cf.core.oagw.guard_plugin.v1~:read";
pub const PERM_GUARD_PLUGIN_DELETE: &str = "gts.cf.core.oagw.guard_plugin.v1~:delete";

pub const PERM_TRANSFORM_PLUGIN_CREATE: &str = "gts.cf.core.oagw.transform_plugin.v1~:create";
pub const PERM_TRANSFORM_PLUGIN_READ: &str = "gts.cf.core.oagw.transform_plugin.v1~:read";
pub const PERM_TRANSFORM_PLUGIN_DELETE: &str = "gts.cf.core.oagw.transform_plugin.v1~:delete";

pub const PERM_PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

/// Check whether a security context may use `permission` (exact match or the
/// unrestricted `*` scope).
#[must_use]
pub fn scope_allows(token_scopes: &[String], permission: &str) -> bool {
    token_scopes.iter().any(|s| s == "*" || s == permission)
}

/// Domain error taxonomy.  The API layer maps each variant to a documented
/// GTS instance id + HTTP status.
#[derive(Debug, Error)]
pub enum DomainError {
    #[error("validation error: {0}")]
    Validation(String),

    #[error("alias conflict: {0}")]
    AliasConflict(String),

    #[error("route match conflict: {0}")]
    RouteConflict(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("no matching route found for {host}")]
    RouteNotFound { host: String },

    #[error("authentication failed: {0}")]
    AuthenticationFailed(String),

    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

    #[error("rate limit exceeded (retry after {retry_after_secs}s)")]
    RateLimitExceeded {
        retry_after_secs: u64,
        limit: u64,
        remaining: u64,
        reset_at_unix: u64,
    },

    #[error("CORS origin not allowed: {0}")]
    CorsOriginNotAllowed(String),

    #[error("CORS method not allowed: {0}")]
    CorsMethodNotAllowed(String),

    #[error("plugin in use")]
    PluginInUse {
        plugin_id: String,
        upstreams: Vec<String>,
        routes: Vec<String>,
    },

    #[error("X-OAGW-Target-Host required to disambiguate multi-endpoint upstream")]
    MissingTargetHost { valid_hosts: Vec<String> },

    #[error("invalid target host: {0}")]
    InvalidTargetHost(String),

    #[error("unknown target host: {invalid_value}")]
    UnknownTargetHost {
        invalid_value: String,
        valid_hosts: Vec<String>,
    },

    #[error("referenced secret not found: {0}")]
    SecretNotFound(String),

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("downstream error: {0}")]
    DownstreamError(String),

    #[error("stream aborted: {0}")]
    StreamAborted(String),

    #[error("upstream link unavailable: {0}")]
    LinkUnavailable(String),

    #[error("circuit breaker open")]
    CircuitBreakerOpen,

    #[error("plugin not found: {0}")]
    PluginNotFound(String),

    #[error("connection timeout")]
    ConnectionTimeout,

    #[error("request timeout")]
    RequestTimeout,

    #[error("idle timeout")]
    IdleTimeout,

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("upstream disabled: {0}")]
    UpstreamDisabled(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// GTS instance identifier used in the problem `type` field.
    #[must_use]
    pub fn gts_id(&self) -> &'static str {
        match self {
            Self::Validation(_) | Self::NotFound(_) | Self::PermissionDenied(_) => {
                GTS_VALIDATION_ERROR
            }
            Self::AliasConflict(_) | Self::RouteConflict(_) => GTS_ALIAS_CONFLICT,
            Self::RouteNotFound { .. } => GTS_ROUTE_NOT_FOUND,
            Self::AuthenticationFailed(_) => GTS_AUTHENTICATION_FAILED,
            Self::PayloadTooLarge(_) => GTS_PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => GTS_RATE_LIMIT_EXCEEDED,
            Self::CorsOriginNotAllowed(_) => GTS_CORS_ORIGIN_NOT_ALLOWED,
            Self::CorsMethodNotAllowed(_) => GTS_CORS_METHOD_NOT_ALLOWED,
            Self::PluginInUse { .. } => GTS_PLUGIN_IN_USE,
            Self::MissingTargetHost { .. } => GTS_MISSING_TARGET_HOST,
            Self::InvalidTargetHost(_) => GTS_INVALID_TARGET_HOST,
            Self::UnknownTargetHost { .. } => GTS_UNKNOWN_TARGET_HOST,
            Self::SecretNotFound(_) => GTS_SECRET_NOT_FOUND,
            Self::ProtocolError(_) => GTS_PROTOCOL_ERROR,
            Self::DownstreamError(_) => GTS_DOWNSTREAM_ERROR,
            Self::StreamAborted(_) => GTS_STREAM_ABORTED,
            Self::LinkUnavailable(_) | Self::UpstreamDisabled(_) => GTS_LINK_UNAVAILABLE,
            Self::CircuitBreakerOpen => GTS_CIRCUIT_BREAKER_OPEN,
            Self::PluginNotFound(_) => GTS_PLUGIN_NOT_FOUND,
            Self::ConnectionTimeout => GTS_CONNECTION_TIMEOUT,
            Self::RequestTimeout => GTS_REQUEST_TIMEOUT,
            Self::IdleTimeout => GTS_IDLE_TIMEOUT,
            Self::Internal(_) => GTS_INTERNAL_ERROR,
        }
    }

    /// HTTP status for the problem.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::Validation(_)
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost(_)
            | Self::UnknownTargetHost { .. } => 400,
            Self::AliasConflict(_) | Self::RouteConflict(_) | Self::PluginInUse { .. } => 409,
            Self::NotFound(_) | Self::RouteNotFound { .. } => 404,
            Self::AuthenticationFailed(_) => 401,
            Self::PayloadTooLarge(_) => 413,
            Self::RateLimitExceeded { .. } => 429,
            Self::CorsOriginNotAllowed(_)
            | Self::CorsMethodNotAllowed(_)
            | Self::PermissionDenied(_) => 403,
            Self::SecretNotFound(_) | Self::Internal(_) => 500,
            Self::ProtocolError(_) | Self::DownstreamError(_) | Self::StreamAborted(_) => 502,
            Self::LinkUnavailable(_)
            | Self::CircuitBreakerOpen
            | Self::PluginNotFound(_)
            | Self::UpstreamDisabled(_) => 503,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    /// Short human title for the problem document.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_) => "Validation Error",
            Self::AliasConflict(_) => "Alias Conflict",
            Self::RouteConflict(_) => "Route Match Conflict",
            Self::NotFound(_) => "Not Found",
            Self::RouteNotFound { .. } => "Route Not Found",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::PayloadTooLarge(_) => "Payload Too Large",
            Self::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            Self::CorsOriginNotAllowed(_) => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed(_) => "CORS Method Not Allowed",
            Self::PluginInUse { .. } => "Plugin In Use",
            Self::MissingTargetHost { .. } => "Missing Target Host",
            Self::InvalidTargetHost(_) => "Invalid Target Host",
            Self::UnknownTargetHost { .. } => "Unknown Target Host",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::ProtocolError(_) => "Protocol Error",
            Self::DownstreamError(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::LinkUnavailable(_) => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
            Self::PermissionDenied(_) => "Permission Denied",
            Self::UpstreamDisabled(_) => "Upstream Disabled",
            Self::Internal(_) => "Internal Error",
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn internal_error_uses_distinct_500_gts_id() {
        // Regression: `Internal` previously re-used GTS_PROTOCOL_ERROR (a 502
        // type) while reporting status 500 — a type/status divergence.  It now
        // carries its own id whose documented status matches 500.
        let e = DomainError::Internal("boom".into());
        assert_eq!(e.gts_id(), GTS_INTERNAL_ERROR);
        assert_ne!(e.gts_id(), GTS_PROTOCOL_ERROR);
        assert_eq!(e.status(), 500);
    }

    #[test]
    fn every_error_reports_documented_type_and_status() {
        // Spot-check the full taxonomy stays aligned with the DESIGN error
        // table: each variant's gts_id must belong either to OAGW or to the
        // canonical family, and statuses keep their documented values.
        assert_eq!(DomainError::ProtocolError("x".into()).status(), 502);
        assert_eq!(DomainError::DownstreamError("x".into()).status(), 502);
        assert_eq!(DomainError::LinkUnavailable("x".into()).status(), 503);
        assert_eq!(DomainError::Internal("x".into()).status(), 500);
        assert_eq!(DomainError::Validation("x".into()).status(), 400);
        assert!(
            DomainError::Internal("x".into())
                .gts_id()
                .starts_with("gts.cf.core.errors.err.v1~")
        );
    }
}
