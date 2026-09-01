//! Domain error type for the OAGW gear.
//!
//! Every variant maps one-to-one onto an RFC 9457 problem response carrying a
//! GTS `type` id (see `domain/gts_helpers.rs`), a stable title, an HTTP
//! status and OAGW-specific extension fields. The REST layer converts these
//! into `OagwProblem` bodies with `X-OAGW-Error-Source: gateway`.

use serde::Serialize;

use super::gts_helpers::{
    ERROR_AUTH_FAILED, ERROR_CIRCUIT_BREAKER_OPEN, ERROR_CORS_METHOD_NOT_ALLOWED,
    ERROR_CORS_ORIGIN_NOT_ALLOWED, ERROR_DOWNSTREAM, ERROR_INVALID_TARGET_HOST,
    ERROR_LINK_UNAVAILABLE, ERROR_MISSING_TARGET_HOST, ERROR_PAYLOAD_TOO_LARGE,
    ERROR_PLUGIN_IN_USE, ERROR_PLUGIN_NOT_FOUND, ERROR_PROTOCOL, ERROR_RATE_LIMIT_EXCEEDED,
    ERROR_ROUTE_NOT_FOUND, ERROR_SECRET_NOT_FOUND, ERROR_STREAM_ABORTED, ERROR_TIMEOUT_CONNECTION,
    ERROR_TIMEOUT_REQUEST, ERROR_UNKNOWN_TARGET_HOST, ERROR_VALIDATION,
};

/// Upstream / route references for a `409 PluginInUse` response.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct ReferencedBy {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
}

/// Contextual extensions attached to problem responses (RFC 9457 extension
/// members, DESIGN §3.3).
#[derive(Debug, Clone, Default)]
pub struct ProblemContext {
    /// Upstream resource id (`gts.cf.core.oagw.upstream.v1~{uuid}`).
    pub upstream_id: Option<String>,
    /// Alias that the request resolved against.
    pub alias: Option<String>,
    /// Resolved upstream host.
    pub host: Option<String>,
    /// Request path (as presented to the proxy).
    pub path: Option<String>,
    /// Retry guidance in seconds.
    pub retry_after_seconds: Option<u64>,
    /// `X-RateLimit-Limit` (bucket capacity) — carried on rate-limit rejections
    /// so the transport layer can emit the standard headers.
    pub rate_limit_limit: Option<u64>,
    /// `X-RateLimit-Remaining`.
    pub rate_limit_remaining: Option<u64>,
    /// `X-RateLimit-Reset` (unix seconds when the bucket refills).
    pub rate_limit_reset: Option<u64>,
    /// Correlation / trace id.
    pub trace_id: Option<String>,
    /// Plugin id implicated in the failure.
    pub plugin_id: Option<String>,
    /// Valid endpoint hosts for `X-OAGW-Target-Host` routing errors.
    pub valid_hosts: Option<Vec<String>>,
    /// Value that was rejected (e.g. an invalid target host value).
    pub invalid_value: Option<String>,
    /// Referenced-by report for plugin-in-use conflicts.
    pub referenced_by: Option<ReferencedBy>,
}

impl ProblemContext {
    /// Drops explicit optional fields (used when no request context exists).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Domain error — the single error type flowing from the control plane and
/// data plane services up to the REST handler layer.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("validation error: {detail}")]
    Validation {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("alias conflict: {detail}")]
    AliasConflict {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("route match conflict: {detail}")]
    RouteConflict {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("resource not found: {detail}")]
    NotFound {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("plugin in use: {detail}")]
    PluginInUse {
        detail: String,
        plugin_id: String,
        referenced_by: ReferencedBy,
    },
    #[error("route not found: {detail}")]
    RouteNotFound {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("authentication failed: {detail}")]
    AuthFailed {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("missing target host: {detail}")]
    MissingTargetHost {
        detail: String,
        valid_hosts: Vec<String>,
        context: Option<ProblemContext>,
    },
    #[error("invalid target host: {detail}")]
    InvalidTargetHost {
        detail: String,
        value: String,
        context: Option<ProblemContext>,
    },
    #[error("unknown target host: {detail}")]
    UnknownTargetHost {
        detail: String,
        value: String,
        valid_hosts: Vec<String>,
        context: Option<ProblemContext>,
    },
    #[error("cors origin not allowed: {detail}")]
    CorsOriginNotAllowed {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("cors method not allowed: {detail}")]
    CorsMethodNotAllowed {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("payload too large: {detail}")]
    PayloadTooLarge {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("rate limit exceeded: {detail}")]
    RateLimitExceeded {
        detail: String,
        retry_after_seconds: u64,
        context: Option<ProblemContext>,
    },
    #[error("secret not found: {detail}")]
    SecretNotFound {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("protocol error: {detail}")]
    ProtocolError {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("downstream error: {detail}")]
    DownstreamError {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("stream aborted: {detail}")]
    StreamAborted {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("link unavailable: {detail}")]
    LinkUnavailable {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("circuit breaker open: {detail}")]
    CircuitBreakerOpen {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("plugin not found: {detail}")]
    PluginNotFound {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("timeout: {detail}")]
    TimeoutConnection {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("timeout: {detail}")]
    TimeoutRequest {
        detail: String,
        context: Option<ProblemContext>,
    },
    #[error("internal error: {detail}")]
    Internal {
        detail: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

/// Compact problem descriptor produced by [`DomainError::problem_info`].
#[derive(Debug, Clone)]
pub struct ProblemInfo {
    /// Full GTS error `type` id.
    pub type_id: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// HTTP status code.
    pub status: u16,
    /// Human-readable detail for this occurrence.
    pub detail: String,
    /// OAGW-specific extension context.
    pub context: Option<ProblemContext>,
}

impl DomainError {
    /// Convenience validation error with no context.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
            context: None,
        }
    }

    /// Convenience 404 with no context.
    #[must_use]
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::NotFound {
            detail: detail.into(),
            context: None,
        }
    }

    /// Convenience 500 with an optional source error (never forwarded over
    /// the wire).
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
            source: None,
        }
    }

    /// Map the error onto its wire problem descriptor.
    ///
    /// The match enumerates every variant with its GTS `type` id / title /
    /// status, so the function is intrinsically long.
    #[allow(clippy::too_many_lines)]
    #[must_use]
    pub fn problem_info(&self) -> ProblemInfo {
        match self {
            Self::Validation { detail, context } => ProblemInfo {
                type_id: ERROR_VALIDATION,
                title: "Validation Error",
                status: 400,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::AliasConflict { detail, context } => ProblemInfo {
                type_id: ERROR_VALIDATION,
                title: "Alias Conflict",
                status: 409,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::RouteConflict { detail, context } => ProblemInfo {
                type_id: ERROR_VALIDATION,
                title: "Route Match Conflict",
                status: 409,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::NotFound { detail, context } => ProblemInfo {
                type_id: ERROR_ROUTE_NOT_FOUND,
                title: "Not Found",
                status: 404,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::PluginInUse {
                detail,
                plugin_id,
                referenced_by,
            } => ProblemInfo {
                type_id: ERROR_PLUGIN_IN_USE,
                title: "Plugin In Use",
                status: 409,
                detail: detail.clone(),
                context: Some(ProblemContext {
                    plugin_id: Some(plugin_id.clone()),
                    referenced_by: Some(referenced_by.clone()),
                    ..ProblemContext::new()
                }),
            },
            Self::RouteNotFound { detail, context } => ProblemInfo {
                type_id: ERROR_ROUTE_NOT_FOUND,
                title: "Route Not Found",
                status: 404,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::AuthFailed { detail, context } => ProblemInfo {
                type_id: ERROR_AUTH_FAILED,
                title: "Authentication Failed",
                status: 401,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::MissingTargetHost {
                detail,
                valid_hosts,
                context,
            } => ProblemInfo {
                type_id: ERROR_MISSING_TARGET_HOST,
                title: "Missing Target Host Header",
                status: 400,
                detail: detail.clone(),
                context: Some(ProblemContext {
                    valid_hosts: Some(valid_hosts.clone()),
                    ..context.clone().unwrap_or_default()
                }),
            },
            Self::InvalidTargetHost {
                detail,
                value,
                context,
            } => ProblemInfo {
                type_id: ERROR_INVALID_TARGET_HOST,
                title: "Invalid Target Host Format",
                status: 400,
                detail: detail.clone(),
                context: Some(ProblemContext {
                    invalid_value: Some(value.clone()),
                    ..context.clone().unwrap_or_default()
                }),
            },
            Self::UnknownTargetHost {
                detail,
                value,
                valid_hosts,
                context,
            } => ProblemInfo {
                type_id: ERROR_UNKNOWN_TARGET_HOST,
                title: "Unknown Target Host",
                status: 400,
                detail: detail.clone(),
                context: Some(ProblemContext {
                    valid_hosts: Some(valid_hosts.clone()),
                    invalid_value: Some(value.clone()),
                    ..context.clone().unwrap_or_default()
                }),
            },
            Self::CorsOriginNotAllowed { detail, context } => ProblemInfo {
                type_id: ERROR_CORS_ORIGIN_NOT_ALLOWED,
                title: "CORS Origin Not Allowed",
                status: 403,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::CorsMethodNotAllowed { detail, context } => ProblemInfo {
                type_id: ERROR_CORS_METHOD_NOT_ALLOWED,
                title: "CORS Method Not Allowed",
                status: 403,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::PayloadTooLarge { detail, context } => ProblemInfo {
                type_id: ERROR_PAYLOAD_TOO_LARGE,
                title: "Payload Too Large",
                status: 413,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::RateLimitExceeded {
                detail,
                retry_after_seconds,
                context,
            } => ProblemInfo {
                type_id: ERROR_RATE_LIMIT_EXCEEDED,
                title: "Rate Limit Exceeded",
                status: 429,
                detail: detail.clone(),
                context: Some(ProblemContext {
                    retry_after_seconds: Some(*retry_after_seconds),
                    ..context.clone().unwrap_or_default()
                }),
            },
            Self::SecretNotFound { detail, context } => ProblemInfo {
                type_id: ERROR_SECRET_NOT_FOUND,
                title: "Secret Not Found",
                status: 500,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::ProtocolError { detail, context } => ProblemInfo {
                type_id: ERROR_PROTOCOL,
                title: "Protocol Error",
                status: 502,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::DownstreamError { detail, context } => ProblemInfo {
                type_id: ERROR_DOWNSTREAM,
                title: "Downstream Error",
                status: 502,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::StreamAborted { detail, context } => ProblemInfo {
                type_id: ERROR_STREAM_ABORTED,
                title: "Stream Aborted",
                status: 502,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::LinkUnavailable { detail, context } => ProblemInfo {
                type_id: ERROR_LINK_UNAVAILABLE,
                title: "Link Unavailable",
                status: 503,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::CircuitBreakerOpen { detail, context } => ProblemInfo {
                type_id: ERROR_CIRCUIT_BREAKER_OPEN,
                title: "Circuit Breaker Open",
                status: 503,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::PluginNotFound { detail, context } => ProblemInfo {
                type_id: ERROR_PLUGIN_NOT_FOUND,
                title: "Plugin Not Found",
                status: 503,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::TimeoutConnection { detail, context } => ProblemInfo {
                type_id: ERROR_TIMEOUT_CONNECTION,
                title: "Connection Timeout",
                status: 504,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::TimeoutRequest { detail, context } => ProblemInfo {
                type_id: ERROR_TIMEOUT_REQUEST,
                title: "Request Timeout",
                status: 504,
                detail: detail.clone(),
                context: context.clone(),
            },
            Self::Internal { detail, .. } => ProblemInfo {
                type_id: ERROR_DOWNSTREAM,
                title: "Internal Error",
                status: 500,
                detail: detail.clone(),
                context: None,
            },
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn validation_maps_to_400_validation_error() {
        let err = DomainError::validation("bad alias");
        let info = err.problem_info();
        assert_eq!(info.status, 400);
        assert_eq!(info.type_id, ERROR_VALIDATION);
    }

    #[test]
    fn rate_limit_maps_to_429_with_retry() {
        let err = DomainError::RateLimitExceeded {
            detail: "limit exceeded".into(),
            retry_after_seconds: 15,
            context: Some(ProblemContext::new()),
        };
        let info = err.problem_info();
        assert_eq!(info.status, 429);
        assert_eq!(info.type_id, ERROR_RATE_LIMIT_EXCEEDED);
        assert_eq!(info.context.as_ref().unwrap().retry_after_seconds, Some(15));
    }

    #[test]
    fn plugin_in_use_carries_references() {
        let err = DomainError::PluginInUse {
            detail: "referenced by 1 upstream".into(),
            plugin_id: "gts...~abc".into(),
            referenced_by: ReferencedBy {
                upstreams: vec!["u1".into()],
                routes: vec![],
            },
        };
        let info = err.problem_info();
        assert_eq!(info.status, 409);
        assert_eq!(info.type_id, ERROR_PLUGIN_IN_USE);
        let ctx = info.context.unwrap();
        assert_eq!(ctx.plugin_id.as_deref(), Some("gts...~abc"));
        assert_eq!(ctx.referenced_by.unwrap().upstreams, vec!["u1"]);
    }
}
