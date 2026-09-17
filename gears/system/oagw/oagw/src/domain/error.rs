//! Domain errors for the OAGW gear.
//!
//! Every error carries the GTS `type` identifier from the DESIGN.md
//! "Error Response Format" table, the HTTP status, and enough context to
//! build an RFC 9457 `application/problem+json` body plus the
//! `X-OAGW-Error-Source` header.

use std::fmt;

/// GTS instance identifiers for gateway-originated errors (DESIGN §3.3).
pub mod gts {
    /// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`
    pub const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1`
    pub const MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1`
    pub const INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1`
    pub const UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1`
    pub const AUTHENTICATION_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1`
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1`
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1`
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.permission.denied.v1`
    pub const PERMISSION_DENIED: &str = "gts.cf.core.errors.err.v1~cf.oagw.permission.denied.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1`
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1`
    pub const PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`
    pub const RATE_LIMIT_EXCEEDED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1`
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1`
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1`
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1`
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1`
    pub const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1`
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1`
    pub const PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1`
    pub const CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1`
    pub const REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    /// `gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1`
    pub const IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
}

/// Whether an error is safe to retry (DESIGN "Retriable" column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retryable {
    /// Not retriable.
    No,
    /// Retriable.
    Yes,
    /// Retry decision depends on the upstream response (5xx status).
    Depends,
}

/// Rate-limit response headers for a 429 (RFC 6585 / rate-limit-headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitHeaders {
    /// `X-RateLimit-Limit`
    pub limit: u64,
    /// `X-RateLimit-Remaining`
    pub remaining: u64,
    /// `X-RateLimit-Reset` (unix seconds)
    pub reset_epoch_secs: u64,
    /// `Retry-After` (seconds)
    pub retry_after_secs: u64,
}

/// OAGW domain error.
///
/// The `source` field classifies the origin: `gateway` errors render as
/// `application/problem+json` with `X-OAGW-Error-Source: gateway`;
/// `upstream` errors pass the upstream body through as-is with
/// `X-OAGW-Error-Source: upstream` (see ADR-0007).
#[derive(Debug)]
pub enum OagwError {
    /// 400 — general request/route validation error.
    Validation { detail: String },
    /// 400 — route validation error.
    RouteError { detail: String },
    /// 400 — `X-OAGW-Target-Host` required for multi-endpoint common-suffix upstream.
    MissingTargetHost,
    /// 400 — `X-OAGW-Target-Host` malformed.
    InvalidTargetHost { detail: String },
    /// 400 — `X-OAGW-Target-Host` matches no configured endpoint.
    UnknownTargetHost { value: String },
    /// 401 — authentication to the upstream failed (auth plugin / secret).
    AuthenticationFailed { detail: String },
    /// 404 — no matching route found.
    RouteNotFound { alias: String, path: String },
    /// 403 — CORS origin not allowed (ADR-0004).
    CorsOriginNotAllowed { origin: String },
    /// 403 — CORS method not allowed (ADR-0004).
    CorsMethodNotAllowed { method: String },
    /// 403 — the bearer token lacks a required OAGW permission/scope.
    Forbidden { detail: String },
    /// 404 — management resource not found (tenant-scoped).
    NotFound { resource: String },
    /// 409 — duplicate or conflict (alias uniqueness, match uniqueness, plugin in use).
    Conflict { detail: String },
    /// 409 — plugin in use by upstream/route binding.
    PluginInUse { plugin_id: String },
    /// 413 — request payload exceeds the size limit.
    PayloadTooLarge { limit: usize },
    /// 429 — rate limit exceeded.
    RateLimitExceeded {
        headers: Option<RateLimitHeaders>,
        detail: String,
    },
    /// 500 — referenced secret not found.
    SecretNotFound { detail: String },
    /// 502 — protocol-level error before/at the upstream connection.
    ProtocolError { detail: String },
    /// 502 — upstream returned an error; body passed through unchanged.
    DownstreamError {
        status: u16,
        body: bytes::Bytes,
        headers: Vec<(String, String)>,
    },
    /// 502 — stream connection aborted.
    StreamAborted { detail: String },
    /// 503 — upstream link unavailable (connect refused, DNS failure).
    LinkUnavailable { detail: String },
    /// 503 — circuit breaker open.
    CircuitBreakerOpen { detail: String },
    /// 503 — configured plugin not found / not resolvable.
    PluginNotFound { plugin_ref: String },
    /// 504 — connection timeout.
    ConnectionTimeout,
    /// 504 — request timeout.
    RequestTimeout,
    /// 504 — idle timeout.
    IdleTimeout,
    /// 500 — internal (unexpected) failure.
    Internal { detail: String },
}

impl OagwError {
    /// HTTP status code for this error.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::Validation { .. }
            | Self::RouteError { .. }
            | Self::MissingTargetHost
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => StatusCode::BAD_REQUEST,
            Self::CorsOriginNotAllowed { .. }
            | Self::CorsMethodNotAllowed { .. }
            | Self::Forbidden { .. } => StatusCode::FORBIDDEN,
            Self::AuthenticationFailed { .. } => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound { .. } | Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::Conflict { .. } | Self::PluginInUse { .. } => StatusCode::CONFLICT,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError { .. }
            | Self::DownstreamError { .. }
            | Self::StreamAborted { .. } => StatusCode::BAD_GATEWAY,
            Self::LinkUnavailable { .. }
            | Self::CircuitBreakerOpen { .. }
            | Self::PluginNotFound { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
            Self::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// GTS `type` identifier for this error.
    #[must_use]
    pub fn gts_type(&self) -> &'static str {
        match self {
            Self::Validation { .. } | Self::RouteError { .. } => gts::VALIDATION,
            Self::MissingTargetHost => gts::MISSING_TARGET_HOST,
            Self::InvalidTargetHost { .. } => gts::INVALID_TARGET_HOST,
            Self::UnknownTargetHost { .. } => gts::UNKNOWN_TARGET_HOST,
            Self::AuthenticationFailed { .. } => gts::AUTHENTICATION_FAILED,
            Self::RouteNotFound { .. } => gts::ROUTE_NOT_FOUND,
            Self::NotFound { .. } => gts::ROUTE_NOT_FOUND,
            Self::CorsOriginNotAllowed { .. } => gts::CORS_ORIGIN_NOT_ALLOWED,
            Self::CorsMethodNotAllowed { .. } => gts::CORS_METHOD_NOT_ALLOWED,
            Self::Forbidden { .. } => gts::PERMISSION_DENIED,
            Self::Conflict { .. } | Self::PluginInUse { .. } => gts::PLUGIN_IN_USE,
            Self::PayloadTooLarge { .. } => gts::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => gts::RATE_LIMIT_EXCEEDED,
            Self::SecretNotFound { .. } => gts::SECRET_NOT_FOUND,
            Self::ProtocolError { .. } => gts::PROTOCOL_ERROR,
            Self::DownstreamError { .. } => gts::DOWNSTREAM_ERROR,
            Self::StreamAborted { .. } => gts::STREAM_ABORTED,
            Self::LinkUnavailable { .. } => gts::LINK_UNAVAILABLE,
            Self::CircuitBreakerOpen { .. } => gts::CIRCUIT_BREAKER_OPEN,
            Self::PluginNotFound { .. } => gts::PLUGIN_NOT_FOUND,
            Self::ConnectionTimeout => gts::CONNECTION_TIMEOUT,
            Self::RequestTimeout => gts::REQUEST_TIMEOUT,
            Self::IdleTimeout => gts::IDLE_TIMEOUT,
            Self::Internal { .. } => gts::LINK_UNAVAILABLE,
        }
    }

    /// RFC 9457 `title` for this error.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation { .. } | Self::RouteError { .. } => "Validation failed",
            Self::MissingTargetHost => "Missing X-OAGW-Target-Host header",
            Self::InvalidTargetHost { .. } => "Invalid X-OAGW-Target-Host header",
            Self::UnknownTargetHost { .. } => "Unknown target host",
            Self::AuthenticationFailed { .. } => "Authentication failed",
            Self::RouteNotFound { .. } => "Route not found",
            Self::NotFound { .. } => "Resource not found",
            Self::CorsOriginNotAllowed { .. } => "CORS origin not allowed",
            Self::CorsMethodNotAllowed { .. } => "CORS method not allowed",
            Self::Forbidden { .. } => "Permission denied",
            Self::Conflict { .. } => "Conflict",
            Self::PluginInUse { .. } => "Plugin in use",
            Self::PayloadTooLarge { .. } => "Request payload too large",
            Self::RateLimitExceeded { .. } => "Rate limit exceeded",
            Self::SecretNotFound { .. } => "Referenced secret not found",
            Self::ProtocolError { .. } => "Protocol error",
            Self::DownstreamError { .. } => "Downstream error",
            Self::StreamAborted { .. } => "Stream aborted",
            Self::LinkUnavailable { .. } => "Upstream link unavailable",
            Self::CircuitBreakerOpen { .. } => "Circuit breaker open",
            Self::PluginNotFound { .. } => "Plugin not found",
            Self::ConnectionTimeout => "Connection timeout",
            Self::RequestTimeout => "Request timeout",
            Self::IdleTimeout => "Idle timeout",
            Self::Internal { .. } => "Internal error",
        }
    }

    /// Human-readable `detail` for this error.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::Validation { detail } => detail.clone(),
            Self::RouteError { detail } => detail.clone(),
            Self::MissingTargetHost => {
                "X-OAGW-Target-Host header is required for multi-endpoint upstreams with a common suffix alias"
                    .to_owned()
            }
            Self::InvalidTargetHost { detail } => detail.clone(),
            Self::UnknownTargetHost { value } => {
                format!("X-OAGW-Target-Host value {value:?} does not match any configured endpoint")
            }
            Self::AuthenticationFailed { detail } => detail.clone(),
            Self::RouteNotFound { alias, path } => {
                format!("no matching route for alias {alias:?} path {path:?}")
            }
            Self::NotFound { resource } => format!("{resource} not found"),
            Self::CorsOriginNotAllowed { origin } => {
                format!("Origin {origin:?} not in allowed origins list")
            }
            Self::CorsMethodNotAllowed { method } => {
                format!("Method {method:?} not in allowed methods list")
            }
            Self::Forbidden { detail } => detail.clone(),
            Self::Conflict { detail } => detail.clone(),
            Self::PluginInUse { plugin_id } => {
                format!("plugin {plugin_id} is referenced by upstream or route bindings")
            }
            Self::PayloadTooLarge { limit } => {
                format!("request payload exceeds the configured limit of {limit} bytes")
            }
            Self::RateLimitExceeded { detail, .. } => detail.clone(),
            Self::SecretNotFound { detail } => detail.clone(),
            Self::ProtocolError { detail } => detail.clone(),
            Self::DownstreamError { status, .. } => {
                format!("upstream service returned HTTP {status}")
            }
            Self::StreamAborted { detail } => detail.clone(),
            Self::LinkUnavailable { detail } => detail.clone(),
            Self::CircuitBreakerOpen { detail } => detail.clone(),
            Self::PluginNotFound { plugin_ref } => {
                format!("plugin {plugin_ref:?} is not resolvable")
            }
            Self::ConnectionTimeout => "connection to upstream timed out".to_owned(),
            Self::RequestTimeout => "request to upstream timed out".to_owned(),
            Self::IdleTimeout => "upstream connection idle timeout".to_owned(),
            Self::Internal { detail } => detail.clone(),
        }
    }

    /// Whether the underlying condition is retriable.
    #[must_use]
    pub fn retryable(&self) -> Retryable {
        match self {
            Self::RateLimitExceeded { .. } => Retryable::Yes,
            Self::LinkUnavailable { .. } | Self::CircuitBreakerOpen { .. } => Retryable::Yes,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => Retryable::Yes,
            Self::DownstreamError { status, .. } if *status >= 500 => Retryable::Depends,
            _ => Retryable::No,
        }
    }

    /// Convenience constructors for management-plane errors.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }

    /// Convenience constructor for internal errors. Never includes secrets.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }
}

impl fmt::Display for OagwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.title(), self.detail())
    }
}

impl std::error::Error for OagwError {}

impl From<tokio::time::error::Elapsed> for OagwError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        Self::RequestTimeout
    }
}

/// Alias for crate-internal result type.
pub type OagwResult<T> = Result<T, OagwError>;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use http::StatusCode;

    /// Every error variant must map to the status + GTS type the problem+
    /// json renderer relies on (DESIGN §3.3 error table).
    #[test]
    fn status_and_gts_type_map_per_the_error_table() {
        use http::StatusCode;
        let cases: Vec<(OagwError, StatusCode, &'static str, &'static str)> = vec![
            (
                OagwError::validation("bad input"),
                StatusCode::BAD_REQUEST,
                gts::VALIDATION,
                "Validation failed",
            ),
            (
                OagwError::RouteError {
                    detail: "nope".into(),
                },
                StatusCode::BAD_REQUEST,
                gts::VALIDATION,
                "Validation failed",
            ),
            (
                OagwError::MissingTargetHost,
                StatusCode::BAD_REQUEST,
                gts::MISSING_TARGET_HOST,
                "Missing X-OAGW-Target-Host header",
            ),
            (
                OagwError::InvalidTargetHost {
                    detail: "bad".into(),
                },
                StatusCode::BAD_REQUEST,
                gts::INVALID_TARGET_HOST,
                "Invalid X-OAGW-Target-Host header",
            ),
            (
                OagwError::UnknownTargetHost { value: "x".into() },
                StatusCode::BAD_REQUEST,
                gts::UNKNOWN_TARGET_HOST,
                "Unknown target host",
            ),
            (
                OagwError::AuthenticationFailed {
                    detail: "denied".into(),
                },
                StatusCode::UNAUTHORIZED,
                gts::AUTHENTICATION_FAILED,
                "Authentication failed",
            ),
            (
                OagwError::RouteNotFound {
                    alias: "a".into(),
                    path: "/".into(),
                },
                StatusCode::NOT_FOUND,
                gts::ROUTE_NOT_FOUND,
                "Route not found",
            ),
            (
                OagwError::NotFound {
                    resource: "upstream".into(),
                },
                StatusCode::NOT_FOUND,
                gts::ROUTE_NOT_FOUND,
                "Resource not found",
            ),
            (
                OagwError::CorsOriginNotAllowed { origin: "o".into() },
                StatusCode::FORBIDDEN,
                gts::CORS_ORIGIN_NOT_ALLOWED,
                "CORS origin not allowed",
            ),
            (
                OagwError::CorsMethodNotAllowed {
                    method: "PATCH".into(),
                },
                StatusCode::FORBIDDEN,
                gts::CORS_METHOD_NOT_ALLOWED,
                "CORS method not allowed",
            ),
            (
                OagwError::Forbidden {
                    detail: "scope".into(),
                },
                StatusCode::FORBIDDEN,
                gts::PERMISSION_DENIED,
                "Permission denied",
            ),
            (
                OagwError::Conflict {
                    detail: "dup".into(),
                },
                StatusCode::CONFLICT,
                gts::PLUGIN_IN_USE,
                "Conflict",
            ),
            (
                OagwError::PluginInUse {
                    plugin_id: "p1".into(),
                },
                StatusCode::CONFLICT,
                gts::PLUGIN_IN_USE,
                "Plugin in use",
            ),
            (
                OagwError::PayloadTooLarge { limit: 32 },
                StatusCode::PAYLOAD_TOO_LARGE,
                gts::PAYLOAD_TOO_LARGE,
                "Request payload too large",
            ),
            (
                OagwError::RateLimitExceeded {
                    headers: None,
                    detail: "slow".into(),
                },
                StatusCode::TOO_MANY_REQUESTS,
                gts::RATE_LIMIT_EXCEEDED,
                "Rate limit exceeded",
            ),
            (
                OagwError::ProtocolError {
                    detail: "tls".into(),
                },
                StatusCode::BAD_GATEWAY,
                gts::PROTOCOL_ERROR,
                "Protocol error",
            ),
            (
                OagwError::StreamAborted {
                    detail: "reset".into(),
                },
                StatusCode::BAD_GATEWAY,
                gts::STREAM_ABORTED,
                "Stream aborted",
            ),
            (
                OagwError::LinkUnavailable {
                    detail: "refused".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                gts::LINK_UNAVAILABLE,
                "Upstream link unavailable",
            ),
            (
                OagwError::CircuitBreakerOpen {
                    detail: "open".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                gts::CIRCUIT_BREAKER_OPEN,
                "Circuit breaker open",
            ),
            (
                OagwError::PluginNotFound {
                    plugin_ref: "p".into(),
                },
                StatusCode::SERVICE_UNAVAILABLE,
                gts::PLUGIN_NOT_FOUND,
                "Plugin not found",
            ),
            (
                OagwError::ConnectionTimeout,
                StatusCode::GATEWAY_TIMEOUT,
                gts::CONNECTION_TIMEOUT,
                "Connection timeout",
            ),
            (
                OagwError::RequestTimeout,
                StatusCode::GATEWAY_TIMEOUT,
                gts::REQUEST_TIMEOUT,
                "Request timeout",
            ),
            (
                OagwError::IdleTimeout,
                StatusCode::GATEWAY_TIMEOUT,
                gts::IDLE_TIMEOUT,
                "Idle timeout",
            ),
            (
                OagwError::Internal {
                    detail: "boom".into(),
                },
                StatusCode::INTERNAL_SERVER_ERROR,
                gts::LINK_UNAVAILABLE,
                "Internal error",
            ),
        ];
        for (err, status, gts_id, title) in cases {
            assert_eq!(err.status(), status, "status for {err:?}");
            assert_eq!(err.gts_type(), gts_id, "gts type for {err:?}");
            assert_eq!(err.title(), title, "title for {err:?}");
            assert!(!err.detail().is_empty(), "detail for {err:?}");
        }
    }

    /// Downstream (upstream-sourced) errors carry their own status verbatim.
    #[test]
    fn downstream_error_keeps_upstream_status() {
        let err = OagwError::DownstreamError {
            status: 404,
            body: bytes::Bytes::from_static(b"not here"),
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
        };
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(err.gts_type(), gts::DOWNSTREAM_ERROR);
        assert!(err.detail().contains("404"));
        assert_eq!(
            err.retryable(),
            Retryable::No,
            "4xx downstream is not retriable"
        );

        let err = OagwError::DownstreamError {
            status: 503,
            body: bytes::Bytes::new(),
            headers: Vec::new(),
        };
        assert_eq!(
            err.retryable(),
            Retryable::Depends,
            "5xx downstream depends"
        );
    }

    /// Retryability follows the DESIGN column.
    #[test]
    fn retryable_flags_are_classified() {
        assert_eq!(
            OagwError::RateLimitExceeded {
                headers: None,
                detail: "d".into()
            }
            .retryable(),
            Retryable::Yes
        );
        assert_eq!(
            OagwError::LinkUnavailable { detail: "d".into() }.retryable(),
            Retryable::Yes
        );
        assert_eq!(OagwError::RequestTimeout.retryable(), Retryable::Yes);
        assert_eq!(
            OagwError::CircuitBreakerOpen { detail: "d".into() }.retryable(),
            Retryable::Yes
        );
        assert_eq!(
            OagwError::Validation { detail: "d".into() }.retryable(),
            Retryable::No
        );
        assert_eq!(
            OagwError::AuthenticationFailed { detail: "d".into() }.retryable(),
            Retryable::No
        );
        assert_eq!(
            OagwError::RouteNotFound {
                alias: "a".into(),
                path: "/".into()
            }
            .retryable(),
            Retryable::No
        );
    }

    #[test]
    fn display_and_elapsed_conversion() {
        let err = OagwError::validation("bad schema");
        assert_eq!(err.to_string(), "Validation failed: bad schema");
    }

    #[tokio::test]
    async fn elapsed_convertsto_request_timeout() {
        use std::future::pending;
        let elapsed = tokio::time::timeout(std::time::Duration::from_nanos(1), pending::<()>())
            .await
            .expect_err("zero timeout always elapses");
        let converted: OagwError = elapsed.into();
        assert_eq!(converted.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(converted.gts_type(), gts::REQUEST_TIMEOUT);
        assert_eq!(converted.retryable(), Retryable::Yes);
    }

    #[test]
    fn rate_limit_headers_roundtrip_via_constructor() {
        let headers = RateLimitHeaders {
            limit: 5,
            remaining: 2,
            reset_epoch_secs: 1_700_000_000,
            retry_after_secs: 3,
        };
        let err = OagwError::RateLimitExceeded {
            headers: Some(headers.clone()),
            detail: "limit".to_owned(),
        };
        match &err {
            OagwError::RateLimitExceeded {
                headers: Some(h), ..
            } => assert_eq!(h, &headers),
            other => panic!("unexpected variant {other:?}"),
        }
        assert_eq!(err.status(), StatusCode::TOO_MANY_REQUESTS);
    }
}
