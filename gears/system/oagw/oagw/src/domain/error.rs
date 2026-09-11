//! The single gateway error enum (DESIGN §3.3).
//!
//! One variant per documented error, each carrying its HTTP status, GTS instance
//! id and (when the error is retriable) a `Retry-After` hint. [`api::rest::error`]
//! maps the whole enum to an RFC 9457 `application/problem+json` body in one
//! place, so the status, `type` and `X-OAGW-Error-Source` header can never drift
//! apart.

use std::time::Duration;

use thiserror::Error;

use crate::domain::gts_helpers as gts;

/// Every error the gear produces.
#[derive(Debug, Clone, Error)]
pub enum DomainError {
    /// 400 — general request/route validation failure.
    #[error("{0}")]
    Validation(String),

    /// 400 — `X-OAGW-Target-Host` missing on a multi-endpoint, common-suffix alias.
    #[error("X-OAGW-Target-Host header is required for this upstream")]
    MissingTargetHost,

    /// 400 — `X-OAGW-Target-Host` present but not a bare host.
    #[error("X-OAGW-Target-Host value is invalid: {0}")]
    InvalidTargetHost(String),

    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    #[error("X-OAGW-Target-Host value does not match any configured endpoint: {0}")]
    UnknownTargetHost(String),

    /// 403 — CORS origin is not in `allowed_origins`.
    #[error("origin is not allowed: {0}")]
    CorsOriginNotAllowed(String),

    /// 403 — cross-origin method is not in `allowed_methods`.
    #[error("method is not allowed: {0}")]
    CorsMethodNotAllowed(String),

    /// 401 — outbound authentication to the upstream failed.
    #[error("{0}")]
    AuthenticationFailed(String),

    /// 404 — no upstream/alias/route matched.
    #[error("{0}")]
    NotFound(String),

    /// 404 — the proxy path matched nothing (`RouteNotFound`).
    #[error("{0}")]
    RouteNotFound(String),

    /// 409 — a plugin still referenced by an upstream or route.
    #[error("plugin is referenced by {upstreams} upstream(s) and {routes} route(s)")]
    PluginInUse {
        /// How many upstreams bind the plugin.
        upstreams: usize,
        /// How many routes bind the plugin.
        routes: usize,
        /// GTS ids of the referencing upstreams (ADR 0001 `referenced_by`).
        upstream_ids: Vec<String>,
        /// GTS ids of the referencing routes.
        route_ids: Vec<String>,
    },

    /// 409 — a management uniqueness constraint was violated.
    #[error("{0}")]
    Conflict(String),

    /// 413 — request payload exceeds the hard limit.
    #[error("request payload exceeds the maximum supported size")]
    PayloadTooLarge,

    /// 429 — token bucket exhausted.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Seconds until the next token is available.
        retry_after: u64,
    },

    /// 500 — a `secret_ref` could not be resolved from the credential store.
    #[error("referenced secret could not be resolved")]
    SecretNotFound,

    /// 502 — protocol-level failure while talking to the upstream.
    #[error("{0}")]
    ProtocolError(String),

    /// 502 — the upstream service failed.
    #[error("{0}")]
    DownstreamError(String),

    /// 502 — a stream (SSE/WebSocket) was aborted mid-flight.
    #[error("{0}")]
    StreamAborted(String),

    /// 502 — a guard rejected the upstream response.
    #[error("{0}")]
    DownstreamRejected(String),

    /// 503 — the upstream is disabled or otherwise unreachable by policy.
    #[error("{0}")]
    LinkUnavailable(String),

    /// 503 — circuit breaker open.
    #[error("circuit breaker is open")]
    CircuitBreakerOpen,

    /// 503 — a bound plugin could not be resolved.
    #[error("{0}")]
    PluginNotFound(String),

    /// 504 — connecting to the upstream timed out.
    #[error("connection to the upstream timed out")]
    ConnectionTimeout,

    /// 504 — the upstream did not answer within `proxy_timeout_secs`.
    #[error("the upstream did not respond in time")]
    RequestTimeout,

    /// 504 — an established stream went idle.
    #[error("the upstream stream went idle")]
    IdleTimeout,

    /// 500 — unexpected internal failure.
    #[error("{0}")]
    Internal(String),
}

impl DomainError {
    /// RFC 9457 status code for this error.
    #[must_use]
    pub fn status(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::Validation(_)
            | Self::MissingTargetHost
            | Self::InvalidTargetHost(_)
            | Self::UnknownTargetHost(_) => StatusCode::BAD_REQUEST,
            Self::CorsOriginNotAllowed(_) | Self::CorsMethodNotAllowed(_) => StatusCode::FORBIDDEN,
            Self::AuthenticationFailed(_) => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) | Self::RouteNotFound(_) => StatusCode::NOT_FOUND,
            Self::PluginInUse { .. } | Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::ProtocolError(_)
            | Self::DownstreamError(_)
            | Self::StreamAborted(_)
            | Self::DownstreamRejected(_) => StatusCode::BAD_GATEWAY,
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen | Self::PluginNotFound(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                StatusCode::GATEWAY_TIMEOUT
            }
        }
    }

    /// GTS instance id (the part after `gts.cf.core.errors.err.v1~`).
    ///
    /// One arm per variant on purpose: the table mirrors DESIGN §3.3's error
    /// catalogue, so a variant keeps its own row even when two of them share a
    /// GTS type today.
    #[must_use]
    #[allow(clippy::match_same_arms)]
    pub fn instance_id(&self) -> &'static str {
        match self {
            Self::Validation(_) => gts::ERR_VALIDATION,
            Self::MissingTargetHost => gts::ERR_MISSING_TARGET_HOST,
            Self::InvalidTargetHost(_) => gts::ERR_INVALID_TARGET_HOST,
            Self::UnknownTargetHost(_) => gts::ERR_UNKNOWN_TARGET_HOST,
            Self::CorsOriginNotAllowed(_) => gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            Self::CorsMethodNotAllowed(_) => gts::ERR_CORS_METHOD_NOT_ALLOWED,
            Self::AuthenticationFailed(_) => gts::ERR_AUTH_FAILED,
            Self::NotFound(_) | Self::RouteNotFound(_) => gts::ERR_ROUTE_NOT_FOUND,
            Self::PluginInUse { .. } => gts::ERR_PLUGIN_IN_USE,
            Self::Conflict(_) => gts::ERR_VALIDATION,
            Self::PayloadTooLarge => gts::ERR_PAYLOAD_TOO_LARGE,
            Self::RateLimitExceeded { .. } => gts::ERR_RATE_LIMIT_EXCEEDED,
            Self::SecretNotFound => gts::ERR_SECRET_NOT_FOUND,
            Self::ProtocolError(_) => gts::ERR_PROTOCOL_ERROR,
            Self::DownstreamError(_) => gts::ERR_DOWNSTREAM_ERROR,
            Self::StreamAborted(_) => gts::ERR_STREAM_ABORTED,
            Self::DownstreamRejected(_) => gts::ERR_DOWNSTREAM_ERROR,
            Self::LinkUnavailable(_) | Self::CircuitBreakerOpen | Self::PluginNotFound(_) => {
                gts::ERR_LINK_UNAVAILABLE
            }
            Self::ConnectionTimeout => gts::ERR_TIMEOUT_CONNECTION,
            Self::RequestTimeout => gts::ERR_TIMEOUT_REQUEST,
            Self::IdleTimeout => gts::ERR_TIMEOUT_IDLE,
            Self::Internal(_) => gts::ERR_PROTOCOL_ERROR,
        }
    }

    /// Short human-readable title for the problem body.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_) | Self::Conflict(_) => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host",
            Self::InvalidTargetHost(_) => "Invalid Target Host",
            Self::UnknownTargetHost(_) => "Unknown Target Host",
            Self::CorsOriginNotAllowed(_) => "CORS Origin Not Allowed",
            Self::CorsMethodNotAllowed(_) => "CORS Method Not Allowed",
            Self::AuthenticationFailed(_) => "Authentication Failed",
            Self::NotFound(_) | Self::RouteNotFound(_) => "Route Not Found",
            Self::PluginInUse { .. } => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded { .. } => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::ProtocolError(_) => "Protocol Error",
            Self::DownstreamError(_) | Self::DownstreamRejected(_) => "Downstream Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::LinkUnavailable(_) => "Link Unavailable",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
            Self::Internal(_) => "Internal Error",
        }
    }

    /// `Retry-After` value in seconds, for retriable errors.
    #[must_use]
    #[allow(clippy::match_same_arms)] // every retriable family stays explicit
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimitExceeded { retry_after } => Some(Duration::from_secs(*retry_after)),
            Self::CircuitBreakerOpen | Self::LinkUnavailable(_) => Some(Duration::from_secs(1)),
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => {
                Some(Duration::from_secs(1))
            }
            _ => None,
        }
    }

    /// Whether the operation is safe to retry per DESIGN §3.3.
    #[must_use]
    pub fn retriable(&self) -> bool {
        matches!(
            self,
            Self::RateLimitExceeded { .. }
                | Self::CircuitBreakerOpen
                | Self::LinkUnavailable(_)
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }

    /// Error-specific members appended to the problem body (RFC 9457 allows
    /// extension members). ADR 0001 names the plugin's own GTS id and the
    /// resources still holding a reference for `PluginInUse`.
    #[must_use]
    pub fn members(&self) -> Vec<(&'static str, serde_json::Value)> {
        match self {
            Self::PluginInUse {
                upstream_ids,
                route_ids,
                ..
            } => vec![(
                "referenced_by",
                serde_json::json!({
                    "upstreams": upstream_ids,
                    "routes": route_ids,
                }),
            )],
            _ => Vec::new(),
        }
    }
}

impl From<std::io::Error> for DomainError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(e.to_string())
    }
}
