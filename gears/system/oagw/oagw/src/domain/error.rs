//! Domain errors and their RFC 9457 problem mapping (`DESIGN.md` §3.3).

use serde::Serialize;

/// GTS type prefix shared by every OAGW error identifier.
const ERR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// GTS type identifier of the canonical `already_exists` problem category.
///
/// Used for write conflicts (alias uniqueness, route-match uniqueness) that
/// the OAGW error table does not enumerate: the platform's
/// `cf.core.err.already_exists.v1` type is reused rather than inventing an
/// undocumented OAGW error type.
pub const ERR_ALREADY_EXISTS: &str = "gts.cf.core.errors.err.v1~cf.core.err.already_exists.v1";

/// Fixed `Retry-After` (seconds) carried by the retriable timeout errors.
///
/// `ConnectionTimeout`, `RequestTimeout` and `IdleTimeout` are marked
/// retriable in `DESIGN.md` §3.3, so the 504 problem carries a `Retry-After`
/// header. The gateway does not retry, so the value is a small fixed back-off
/// hint rather than a computed deadline.
pub const TIMEOUT_RETRY_AFTER_SECS: u64 = 1;

/// A resource reference set, as reported by a `409 PluginInUse` problem.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReferencedBy {
    /// Upstream instances referencing the plugin.
    pub upstreams: Vec<String>,
    /// Route instances referencing the plugin.
    pub routes: Vec<String>,
}

/// Every gateway-originated error, with its RFC 9457 problem metadata.
///
/// The 20 contract entries of `DESIGN.md` §3.3 are represented 1:1 (the
/// `RouteError` and `ValidationError` rows share one identifier, so they map
/// to the single [`DomainError::Validation`] variant) plus the two CORS
/// rejection types from ADR-0004 and the canonical `already_exists` conflict.
///
/// Three further variants cover conditions the contract table does not
/// enumerate but the PRD mandates: a disabled upstream (`PRD.md` §5.1
/// "Enable/Disable Semantics" requires 503, not 404), an authorization denial
/// (`DESIGN.md` §3.2) and an unavailable tenant hierarchy (`DESIGN.md` §3.1).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// 400 — general request / configuration validation failure.
    #[error("validation failed: {0}")]
    Validation(String),
    /// 403 — the authorization resolver denied the operation.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// 400 — `X-OAGW-Target-Host` required for a multi-endpoint common-suffix alias.
    #[error("X-OAGW-Target-Host header is required to select an endpoint: {0}")]
    MissingTargetHost(String),
    /// 400 — `X-OAGW-Target-Host` is not a hostname or IP.
    #[error("invalid X-OAGW-Target-Host value: {0}")]
    InvalidTargetHost(String),
    /// 400 — `X-OAGW-Target-Host` matches no configured endpoint.
    #[error("unknown X-OAGW-Target-Host value: {0}")]
    UnknownTargetHost(String),
    /// 401 — authentication with the upstream (or credential resolution) failed.
    #[error("authentication to upstream failed: {0}")]
    AuthenticationFailed(String),
    /// 404 — no enabled route matched the request.
    #[error("no matching route: {0}")]
    RouteNotFound(String),
    /// 409 — plugin still referenced by an upstream or route.
    #[error("plugin in use: {}", detail)]
    PluginInUse {
        /// Human-readable detail.
        detail: String,
        /// GTS instance id of the plugin.
        plugin_id: String,
        /// Referencing resources.
        referenced_by: ReferencedBy,
    },
    /// 413 — request payload above the 100 MB hard limit.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),
    /// 429 — token bucket exhausted.
    #[error("rate limit exceeded: {}", detail)]
    RateLimitExceeded {
        /// Human-readable detail.
        detail: String,
        /// Seconds the client is advised to wait.
        retry_after_seconds: u64,
    },
    /// 409 — alias or route match uniqueness conflict.
    #[error("conflict: {0}")]
    Conflict(String),
    /// 500 — a `cred://` reference resolved to nothing.
    #[error("referenced secret not found: {0}")]
    SecretNotFound(String),
    /// 502 — malformed protocol interaction with the upstream.
    #[error("protocol error: {0}")]
    ProtocolError(String),
    /// 502 — upstream returned an error condition the gateway cannot relay.
    #[error("downstream error: {0}")]
    DownstreamError(String),
    /// 502 — streamed response terminated before completion.
    #[error("stream aborted: {0}")]
    StreamAborted(String),
    /// 503 — connection pool exhausted / upstream link down.
    #[error("upstream link unavailable: {}", detail)]
    LinkUnavailable {
        /// Human-readable detail.
        detail: String,
        /// Seconds the client is advised to wait.
        retry_after_seconds: u64,
    },
    /// 503 — circuit breaker is open for the upstream.
    #[error("circuit breaker open: {}", detail)]
    CircuitBreakerOpen {
        /// Human-readable detail.
        detail: String,
        /// Seconds until half-open probing resumes.
        retry_after_seconds: u64,
    },
    /// 503 — plugin reference unresolvable.
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    /// 503 — the alias resolves to an upstream that is disabled, either in the
    /// caller's tenant or in an ancestor (`PRD.md` §5.1: an ancestor-disabled
    /// upstream is disabled for every descendant).
    #[error("upstream disabled: {}", alias)]
    UpstreamDisabled {
        /// Normalized alias of the disabled upstream.
        alias: String,
        /// Seconds the client is advised to wait before re-resolving.
        retry_after_seconds: u64,
    },
    /// 503 — the tenant hierarchy could not be resolved.
    ///
    /// Ancestor `enforce` constraints are safety-critical, so a failed tenant
    /// lookup never degrades to a truncated chain.
    #[error("tenant hierarchy unavailable: {0}")]
    TenantResolution(String),
    /// 504 — TCP/TLS connection phase exceeded the timeout.
    #[error("connection timeout: {0}")]
    ConnectionTimeout(String),
    /// 504 — response head not received within the timeout.
    #[error("request timeout: {0}")]
    RequestTimeout(String),
    /// 504 — streamed response body idle for longer than the timeout.
    #[error("idle timeout: {0}")]
    IdleTimeout(String),
    /// 403 — CORS actual request from an origin outside `allowed_origins`.
    #[error("origin not allowed: {0}")]
    CorsOriginNotAllowed(String),
    /// 403 — CORS actual request whose method is outside `allowed_methods`.
    #[error("method not allowed: {0}")]
    CorsMethodNotAllowed(String),
}

impl DomainError {
    /// The GTS error type identifier, title, HTTP status and machine-readable
    /// code for this error, per the `DESIGN.md` §3.3 contract table.
    #[must_use]
    pub fn descriptor(&self) -> ErrorDescriptor {
        let (type_id, title, status, error_code) = match self {
            Self::Validation(_) => (
                "cf.oagw.validation.error.v1",
                "Validation Error",
                400,
                "VALIDATION_FAILED",
            ),
            Self::Forbidden(_) => ("cf.oagw.forbidden.v1", "Forbidden", 403, "FORBIDDEN"),
            Self::MissingTargetHost(_) => (
                "cf.oagw.routing.missing_target_host.v1",
                "Missing Target Host",
                400,
                "MISSING_TARGET_HOST",
            ),
            Self::InvalidTargetHost(_) => (
                "cf.oagw.routing.invalid_target_host.v1",
                "Invalid Target Host",
                400,
                "INVALID_TARGET_HOST",
            ),
            Self::UnknownTargetHost(_) => (
                "cf.oagw.routing.unknown_target_host.v1",
                "Unknown Target Host",
                400,
                "UNKNOWN_TARGET_HOST",
            ),
            Self::AuthenticationFailed(_) => (
                "cf.oagw.auth.failed.v1",
                "Authentication Failed",
                401,
                "AUTHENTICATION_FAILED",
            ),
            Self::RouteNotFound(_) => (
                "cf.oagw.route.not_found.v1",
                "Route Not Found",
                404,
                "ROUTE_NOT_FOUND",
            ),
            Self::PluginInUse { .. } => (
                "cf.oagw.plugin.in_use.v1",
                "Plugin In Use",
                409,
                "PLUGIN_IN_USE",
            ),
            Self::PayloadTooLarge(_) => (
                "cf.oagw.payload.too_large.v1",
                "Payload Too Large",
                413,
                "PAYLOAD_TOO_LARGE",
            ),
            Self::RateLimitExceeded { .. } => (
                "cf.oagw.rate_limit.exceeded.v1",
                "Rate Limit Exceeded",
                429,
                "RATE_LIMIT_EXCEEDED",
            ),
            Self::Conflict(_) => (
                "cf.core.err.already_exists.v1",
                "Already Exists",
                409,
                "ALREADY_EXISTS",
            ),
            Self::SecretNotFound(_) => (
                "cf.oagw.secret.not_found.v1",
                "Secret Not Found",
                500,
                "SECRET_NOT_FOUND",
            ),
            Self::ProtocolError(_) => (
                "cf.oagw.protocol.error.v1",
                "Protocol Error",
                502,
                "PROTOCOL_ERROR",
            ),
            Self::DownstreamError(_) => (
                "cf.oagw.downstream.error.v1",
                "Downstream Error",
                502,
                "DOWNSTREAM_ERROR",
            ),
            Self::StreamAborted(_) => (
                "cf.oagw.stream.aborted.v1",
                "Stream Aborted",
                502,
                "STREAM_ABORTED",
            ),
            Self::LinkUnavailable { .. } => (
                "cf.oagw.link.unavailable.v1",
                "Link Unavailable",
                503,
                "LINK_UNAVAILABLE",
            ),
            Self::CircuitBreakerOpen { .. } => (
                "cf.oagw.circuit_breaker.open.v1",
                "Circuit Breaker Open",
                503,
                "CIRCUIT_BREAKER_OPEN",
            ),
            Self::PluginNotFound(_) => (
                "cf.oagw.plugin.not_found.v1",
                "Plugin Not Found",
                503,
                "PLUGIN_NOT_FOUND",
            ),
            Self::UpstreamDisabled { .. } => (
                "cf.oagw.upstream.disabled.v1",
                "Upstream Disabled",
                503,
                "UPSTREAM_DISABLED",
            ),
            Self::TenantResolution(_) => (
                "cf.oagw.tenant.unavailable.v1",
                "Tenant Unavailable",
                503,
                "TENANT_UNAVAILABLE",
            ),
            Self::ConnectionTimeout(_) => (
                "cf.oagw.timeout.connection.v1",
                "Connection Timeout",
                504,
                "CONNECTION_TIMEOUT",
            ),
            Self::RequestTimeout(_) => (
                "cf.oagw.timeout.request.v1",
                "Request Timeout",
                504,
                "REQUEST_TIMEOUT",
            ),
            Self::IdleTimeout(_) => (
                "cf.oagw.timeout.idle.v1",
                "Idle Timeout",
                504,
                "IDLE_TIMEOUT",
            ),
            Self::CorsOriginNotAllowed(_) => (
                "cf.oagw.cors.origin_not_allowed.v1",
                "Origin Not Allowed",
                403,
                "CORS_ORIGIN_NOT_ALLOWED",
            ),
            Self::CorsMethodNotAllowed(_) => (
                "cf.oagw.cors.method_not_allowed.v1",
                "Method Not Allowed",
                403,
                "CORS_METHOD_NOT_ALLOWED",
            ),
        };
        ErrorDescriptor {
            type_id: format!("{ERR_TYPE_PREFIX}{type_id}"),
            title,
            status,
            error_code,
            retry_after_seconds: match self {
                Self::RateLimitExceeded {
                    retry_after_seconds,
                    ..
                }
                | Self::LinkUnavailable {
                    retry_after_seconds,
                    ..
                }
                | Self::CircuitBreakerOpen {
                    retry_after_seconds,
                    ..
                }
                | Self::UpstreamDisabled {
                    retry_after_seconds,
                    ..
                } => Some(*retry_after_seconds),
                // The three timeout errors are retriable per DESIGN §3.3, so
                // they carry a small fixed back-off hint.
                Self::ConnectionTimeout(_) | Self::RequestTimeout(_) | Self::IdleTimeout(_) => {
                    Some(TIMEOUT_RETRY_AFTER_SECS)
                }
                _ => None,
            },
        }
    }

    /// Human-readable detail for this error.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Validation(detail)
            | Self::Forbidden(detail)
            | Self::MissingTargetHost(detail)
            | Self::InvalidTargetHost(detail)
            | Self::UnknownTargetHost(detail)
            | Self::AuthenticationFailed(detail)
            | Self::RouteNotFound(detail)
            | Self::Conflict(detail)
            | Self::PayloadTooLarge(detail)
            | Self::SecretNotFound(detail)
            | Self::ProtocolError(detail)
            | Self::DownstreamError(detail)
            | Self::StreamAborted(detail)
            | Self::PluginNotFound(detail)
            | Self::ConnectionTimeout(detail)
            | Self::RequestTimeout(detail)
            | Self::IdleTimeout(detail)
            | Self::CorsOriginNotAllowed(detail)
            | Self::CorsMethodNotAllowed(detail)
            | Self::TenantResolution(detail) => detail.clone(),
            Self::PluginInUse { detail, .. } => detail.clone(),
            Self::RateLimitExceeded { detail, .. }
            | Self::LinkUnavailable { detail, .. }
            | Self::CircuitBreakerOpen { detail, .. } => detail.clone(),
            Self::UpstreamDisabled { alias, .. } => alias.clone(),
        }
    }

    /// True when the error is a client-side (4xx) condition.
    #[must_use]
    pub fn is_client_error(&self) -> bool {
        self.descriptor().status < 500
    }

    /// HTTP status this error maps to.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.descriptor().status
    }
}

/// Wire description of a [`DomainError`] variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorDescriptor {
    /// Full GTS type identifier emitted as the problem `type` field.
    pub type_id: String,
    /// Human-readable summary emitted as the problem `title` field.
    pub title: &'static str,
    /// HTTP status emitted as the problem `status` field.
    pub status: u16,
    /// Machine-readable variant name inside the `oagw` error domain.
    pub error_code: &'static str,
    /// `Retry-After` guidance in seconds, when the error is retriable.
    pub retry_after_seconds: Option<u64>,
}

/// Metric name fragment for the `oagw_errors_total{error_type}` label.
#[must_use]
pub fn error_metric_type(err: &DomainError) -> &'static str {
    err.descriptor().error_code
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn all_contract_status_codes_match_design() {
        let cases: Vec<(DomainError, u16)> = vec![
            (DomainError::Validation("x".into()), 400),
            (DomainError::Forbidden("x".into()), 403),
            (DomainError::MissingTargetHost("x".into()), 400),
            (DomainError::InvalidTargetHost("x".into()), 400),
            (DomainError::UnknownTargetHost("x".into()), 400),
            (DomainError::AuthenticationFailed("x".into()), 401),
            (DomainError::RouteNotFound("x".into()), 404),
            (
                DomainError::PluginInUse {
                    detail: "x".into(),
                    plugin_id: "p".into(),
                    referenced_by: ReferencedBy::default(),
                },
                409,
            ),
            (DomainError::PayloadTooLarge("x".into()), 413),
            (
                DomainError::RateLimitExceeded {
                    detail: "x".into(),
                    retry_after_seconds: 1,
                },
                429,
            ),
            (DomainError::Conflict("x".into()), 409),
            (DomainError::SecretNotFound("x".into()), 500),
            (DomainError::ProtocolError("x".into()), 502),
            (DomainError::DownstreamError("x".into()), 502),
            (DomainError::StreamAborted("x".into()), 502),
            (
                DomainError::LinkUnavailable {
                    detail: "x".into(),
                    retry_after_seconds: 1,
                },
                503,
            ),
            (
                DomainError::CircuitBreakerOpen {
                    detail: "x".into(),
                    retry_after_seconds: 1,
                },
                503,
            ),
            (DomainError::PluginNotFound("x".into()), 503),
            (
                DomainError::UpstreamDisabled {
                    alias: "api.openai.com".into(),
                    retry_after_seconds: 1,
                },
                503,
            ),
            (DomainError::TenantResolution("x".into()), 503),
            (DomainError::ConnectionTimeout("x".into()), 504),
            (DomainError::RequestTimeout("x".into()), 504),
            (DomainError::IdleTimeout("x".into()), 504),
            (DomainError::CorsOriginNotAllowed("x".into()), 403),
            (DomainError::CorsMethodNotAllowed("x".into()), 403),
        ];

        assert_eq!(
            cases.len(),
            25,
            "20 contract errors + 2 CORS errors + already_exists + 2 hierarchy errors"
        );
        for (err, expected_status) in cases {
            let descriptor = err.descriptor();
            assert_eq!(descriptor.status, expected_status, "{descriptor:?}");
            assert!(descriptor.type_id.starts_with("gts.cf.core.errors.err.v1~"));
            assert!(!descriptor.title.is_empty());
        }
    }

    #[test]
    fn retry_after_is_only_reported_for_retriable_errors() {
        assert_eq!(
            DomainError::Validation("x".into())
                .descriptor()
                .retry_after_seconds,
            None
        );
        let rate = DomainError::RateLimitExceeded {
            detail: "x".into(),
            retry_after_seconds: 7,
        };
        assert_eq!(rate.descriptor().retry_after_seconds, Some(7));
    }

    #[test]
    fn timeouts_carry_a_fixed_retry_after() {
        for error in [
            DomainError::ConnectionTimeout("x".into()),
            DomainError::RequestTimeout("x".into()),
            DomainError::IdleTimeout("x".into()),
        ] {
            let descriptor = error.descriptor();
            assert_eq!(descriptor.status, 504, "{descriptor:?}");
            assert_eq!(
                descriptor.retry_after_seconds,
                Some(TIMEOUT_RETRY_AFTER_SECS),
                "{descriptor:?}"
            );
        }
    }

    #[test]
    fn a_disabled_upstream_is_a_503_with_a_retry_after() {
        let error = DomainError::UpstreamDisabled {
            alias: "api.openai.com".into(),
            retry_after_seconds: 30,
        };
        let descriptor = error.descriptor();
        assert_eq!(descriptor.status, 503);
        assert_eq!(descriptor.title, "Upstream Disabled");
        assert_eq!(
            descriptor.type_id,
            "gts.cf.core.errors.err.v1~cf.oagw.upstream.disabled.v1"
        );
        assert_eq!(descriptor.retry_after_seconds, Some(30));
        assert_eq!(error.detail(), "api.openai.com");
        assert!(!error.is_client_error(), "503 is a server-side condition");
    }

    #[test]
    fn forbidden_is_a_403_with_the_documented_type() {
        let descriptor = DomainError::Forbidden("denied".into()).descriptor();
        assert_eq!(descriptor.status, 403);
        assert_eq!(descriptor.title, "Forbidden");
        assert_eq!(
            descriptor.type_id,
            "gts.cf.core.errors.err.v1~cf.oagw.forbidden.v1"
        );
    }

    #[test]
    fn tenant_resolution_is_a_503() {
        let descriptor = DomainError::TenantResolution("resolver down".into()).descriptor();
        assert_eq!(descriptor.status, 503);
        assert_eq!(
            descriptor.type_id,
            "gts.cf.core.errors.err.v1~cf.oagw.tenant.unavailable.v1"
        );
    }
}
