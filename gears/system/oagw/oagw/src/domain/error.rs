//! Domain errors for the OAGW control plane and data plane.
//!
//! * [`DomainError`] — control-plane failures (CRUD / validation /
//!   hierarchy). Mapped to canonical `Problem` envelopes at the REST
//!   boundary via `infra::error_mapping`.
//! * [`DataPlaneError`] — proxy-path failures. These carry the exact
//!   `cf.oagw.*` GTS error type, the HTTP status, retryability and the
//!   RFC 9457 extension fields documented in `DOCS §8`, and compile to a
//!   ready-to-send `application/problem+json` response.

use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use crate::gts_helpers;

// ---------------------------------------------------------------------------
// Control plane
// ---------------------------------------------------------------------------

/// References to resources still binding a plugin (DELETE plugin in-use).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PluginReferences {
    /// Full anonymous GTS ids of referencing upstreams.
    pub upstreams: Vec<String>,
    /// Full anonymous GTS ids of referencing routes.
    pub routes: Vec<String>,
}

impl PluginReferences {
    /// Whether any resource still references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

/// Control-plane domain error.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    /// Validation failure (HTTP 400, `invalid_argument` canonical).
    #[error("validation failed: {detail}")]
    Validation { detail: String },

    /// Resource not found or not visible to the caller (HTTP 404).
    #[error("{resource} not found: {detail}")]
    NotFound { detail: String, resource: String },

    /// Resource already exists (HTTP 409, e.g. alias collision).
    #[error("{detail}")]
    AlreadyExists { detail: String },

    /// Operation conflicts with the current state (HTTP 409, e.g.
    /// deleting an upstream that routes still reference).
    #[error("operation aborted: {detail}")]
    Aborted { detail: String },

    /// Cross-tenant or PDP denial (HTTP 403).
    #[error("access denied: {detail}")]
    CrossTenantDenied { detail: String },

    /// Plugin is still referenced (HTTP 409, `cf.oagw.plugin.in_use`).
    #[error("plugin in use: {detail}")]
    PluginInUse {
        detail: String,
        plugin_id: String,
        referenced_by: PluginReferences,
    },

    /// Upstream or plugin resolution failed (HTTP 500).
    #[error("resolving secret plugin/upstream configuration failed: {detail}")]
    ResolutionFailed { detail: String },

    /// The referenced secret does not exist (HTTP 500
    /// `cf.oagw.secret.not_found`, surfaced on the proxy/auth path).
    #[error("referenced secret not found: {detail}")]
    SecretNotFound { detail: String },

    /// A downstream dependency is unavailable (HTTP 503).
    #[error("service unavailable: {detail}")]
    ServiceUnavailable {
        detail: String,
        retry_after: Option<Duration>,
    },

    /// Operation not supported (HTTP 501).
    #[error("unsupported operation: {detail}")]
    UnsupportedOperation { detail: String },

    /// Internal invariant broken (HTTP 500).
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// Build a `Validation` variant from a displayable message.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }

    /// Build a `NotFound` variant for a resource type.
    #[must_use]
    pub fn not_found(resource: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::NotFound {
            detail: detail.into(),
            resource: resource.into(),
        }
    }

    #[must_use]
    pub fn already_exists(resource: impl Into<String>, detail: impl Into<String>) -> Self {
        let resource = resource.into();
        let detail = detail.into();
        if resource.is_empty() {
            Self::AlreadyExists { detail }
        } else {
            Self::AlreadyExists {
                detail: format!("{resource}: {detail}"),
            }
        }
    }

    #[must_use]
    pub fn aborted(detail: impl Into<String>) -> Self {
        Self::Aborted {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn denied(detail: impl Into<String>) -> Self {
        Self::CrossTenantDenied {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn resolve_failed(detail: impl Into<String>) -> Self {
        Self::ResolutionFailed {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn plugin_in_use(id: Uuid, referenced_by: PluginReferences) -> Self {
        let detail = format!("plugin '{id}' is still referenced and cannot be deleted");
        Self::PluginInUse {
            detail,
            plugin_id: id.to_string(),
            referenced_by,
        }
    }

    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal(detail.into())
    }
}

impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    /// Map PEP enforcement failures into the domain error model,
    /// fail-closed (mirrors AM): denied / un-compilable decisions are
    /// 403 Forbidden; an unreachable PDP is 503 Service Unavailable.
    fn from(err: authz_resolver_sdk::EnforcerError) -> Self {
        use authz_resolver_sdk::EnforcerError;
        match err {
            EnforcerError::Denied { .. } => Self::CrossTenantDenied {
                detail: "authorization denied".to_owned(),
            },
            EnforcerError::EvaluationFailed(_) => Self::ServiceUnavailable {
                detail: "authorization evaluation failed".to_owned(),
                retry_after: None,
            },
            EnforcerError::CompileFailed(_) => Self::CrossTenantDenied {
                detail: "authorization decision could not be enforced".to_owned(),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Data plane
// ---------------------------------------------------------------------------

/// Routing / enforcement outcome for `X-OAGW-Target-Host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetHostOutcome {
    /// Single-endpoint upstream: optional, but validated if present.
    SingleEndpoint,
    /// Header routed to a specific pool member.
    Selected { host: String },
    /// No header and none required.
    None,
}

/// RFC 9457 extension payload for OAGW proxy errors.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ErrorExtensions {
    /// Upstream identifier (request context).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<Uuid>,
    /// Request path (request context).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Rate-limit / retry guidance in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Alias (routing errors).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Valid endpoint hosts for routing errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    /// The offending value for `invalid_target_host` / `unknown_target_host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
}

impl ErrorExtensions {
    #[must_use]
    pub fn with_upstream_id(mut self, id: Uuid) -> Self {
        self.upstream_id = Some(id);
        self
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    #[must_use]
    pub fn with_valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.valid_hosts = Some(hosts);
        self
    }

    #[must_use]
    pub fn with_invalid_value(mut self, value: impl Into<String>) -> Self {
        self.invalid_value = Some(value.into());
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }
}

/// Data-plane (proxy) error, carrying the exact GTS error type and RFC 9457
/// shape described in `DOCS §8`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DataPlaneError {
    #[error("validation error: {detail}")]
    Validation {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("missing X-OAGW-Target-Host")]
    MissingTargetHost {
        valid_hosts: Vec<String>,
        extensions: ErrorExtensions,
    },

    #[error("invalid X-OAGW-Target-Host: {value}")]
    InvalidTargetHost {
        value: String,
        valid_hosts: Vec<String>,
        extensions: ErrorExtensions,
    },

    #[error("unknown X-OAGW-Target-Host: {value}")]
    UnknownTargetHost {
        value: String,
        valid_hosts: Vec<String>,
        extensions: ErrorExtensions,
    },

    #[error("upstream authentication failed: {detail}")]
    AuthFailed {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("access denied: {detail}")]
    Forbidden {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("no route matched: {detail}")]
    RouteNotFound {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("plugin not found or not bindable: {detail}")]
    PluginNotFound {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("payload too large: {detail}")]
    PayloadTooLarge {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("rate limit exceeded")]
    RateLimitExceeded {
        retry_after_seconds: u64,
        extensions: ErrorExtensions,
    },

    #[error("referenced secret missing: {detail}")]
    SecretNotFound {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("protocol error: {detail}")]
    Protocol {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("downstream error: {detail}")]
    Downstream {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("stream aborted: {detail}")]
    StreamAborted {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("upstream link unavailable: {detail}")]
    LinkUnavailable {
        detail: String,
        retry_after: Option<Duration>,
        extensions: ErrorExtensions,
    },

    #[error("circuit breaker open")]
    CircuitBreakerOpen {
        extensions: ErrorExtensions,
    },

    #[error("connection timeout: {detail}")]
    ConnectionTimeout {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("request timeout: {detail}")]
    RequestTimeout {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("idle timeout: {detail}")]
    IdleTimeout {
        detail: String,
        extensions: ErrorExtensions,
    },

    #[error("cors origin not allowed: {origin}")]
    CorsOriginNotAllowed {
        origin: String,
        extensions: ErrorExtensions,
    },

    #[error("cors method not allowed: {method}")]
    CorsMethodNotAllowed {
        method: String,
        extensions: ErrorExtensions,
    },

    #[error("internal error: {detail}")]
    Internal {
        detail: String,
        extensions: ErrorExtensions,
    },
}

impl DataPlaneError {
    /// HTTP status for the error (DOCS §8.1).
    #[must_use]
    pub fn status(&self) -> u16 {
        use DataPlaneError::{
    AuthFailed, CircuitBreakerOpen, ConnectionTimeout, CorsMethodNotAllowed,
    CorsOriginNotAllowed, Downstream, Forbidden, IdleTimeout, Internal,
    InvalidTargetHost, LinkUnavailable, MissingTargetHost, PayloadTooLarge, PluginNotFound,
    Protocol, RateLimitExceeded, RequestTimeout, RouteNotFound, SecretNotFound,
    StreamAborted, UnknownTargetHost, Validation,
};
        match self {
            Validation { .. }
            | MissingTargetHost { .. }
            | InvalidTargetHost { .. }
            | UnknownTargetHost { .. } => 400,
            AuthFailed { .. } => 401,
            Forbidden { .. } | CorsOriginNotAllowed { .. } | CorsMethodNotAllowed { .. } => 403,
            RouteNotFound { .. } => 404,
            PayloadTooLarge { .. } => 413,
            RateLimitExceeded { .. } => 429,
            PluginNotFound { .. }
            | LinkUnavailable { .. }
            | CircuitBreakerOpen { .. }
            | SecretNotFound { .. } => 503,
            Protocol { .. } | Downstream { .. } | StreamAborted { .. } => 502,
            ConnectionTimeout { .. } | RequestTimeout { .. } | IdleTimeout { .. } => 504,
            Internal { .. } => 500,
        }
    }

    /// Retryable per the DOCS §8.1 table (only rate-limit / unavailable /
    /// timeout / breaker-open entries are retryable).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            DataPlaneError::RateLimitExceeded { .. }
                | DataPlaneError::LinkUnavailable { .. }
                | DataPlaneError::CircuitBreakerOpen { .. }
                | DataPlaneError::ConnectionTimeout { .. }
                | DataPlaneError::RequestTimeout { .. }
                | DataPlaneError::IdleTimeout { .. }
        )
    }

    /// The GTS error-type fragment (without the `gts://` scheme).
    #[must_use]
    pub fn error_type(&self) -> &'static str {
        use DataPlaneError::{
    AuthFailed, CircuitBreakerOpen, ConnectionTimeout, CorsMethodNotAllowed,
    CorsOriginNotAllowed, Downstream, Forbidden, IdleTimeout, Internal,
    InvalidTargetHost, LinkUnavailable, MissingTargetHost, PayloadTooLarge, PluginNotFound,
    Protocol, RateLimitExceeded, RequestTimeout, RouteNotFound, SecretNotFound,
    StreamAborted, UnknownTargetHost, Validation,
};
        match self {
            Validation { .. } | Internal { .. } => gts_helpers::ERR_VALIDATION,
            MissingTargetHost { .. } => gts_helpers::ERR_MISSING_TARGET_HOST,
            InvalidTargetHost { .. } => gts_helpers::ERR_INVALID_TARGET_HOST,
            UnknownTargetHost { .. } => gts_helpers::ERR_UNKNOWN_TARGET_HOST,
            AuthFailed { .. } => gts_helpers::ERR_AUTH_FAILED,
            Forbidden { .. } => gts_helpers::ERR_PERMISSION_DENIED,
            RouteNotFound { .. } => gts_helpers::ERR_ROUTE_NOT_FOUND,
            PluginNotFound { .. } => gts_helpers::ERR_PLUGIN_NOT_FOUND,
            PayloadTooLarge { .. } => gts_helpers::ERR_PAYLOAD_TOO_LARGE,
            RateLimitExceeded { .. } => gts_helpers::ERR_RATE_LIMIT_EXCEEDED,
            SecretNotFound { .. } => gts_helpers::ERR_SECRET_NOT_FOUND,
            Protocol { .. } => gts_helpers::ERR_PROTOCOL,
            Downstream { .. } => gts_helpers::ERR_DOWNSTREAM,
            StreamAborted { .. } => gts_helpers::ERR_STREAM_ABORTED,
            LinkUnavailable { .. } => gts_helpers::ERR_LINK_UNAVAILABLE,
            CircuitBreakerOpen { .. } => gts_helpers::ERR_CIRCUIT_BREAKER_OPEN,
            ConnectionTimeout { .. } => gts_helpers::ERR_TIMEOUT_CONNECTION,
            RequestTimeout { .. } => gts_helpers::ERR_TIMEOUT_REQUEST,
            IdleTimeout { .. } => gts_helpers::ERR_TIMEOUT_IDLE,
            CorsOriginNotAllowed { .. } => gts_helpers::ERR_CORS_ORIGIN_NOT_ALLOWED,
            CorsMethodNotAllowed { .. } => gts_helpers::ERR_CORS_METHOD_NOT_ALLOWED,
        }
    }

    /// Human-readable RFC 9457 `title` (DOCS §8.2).
    #[must_use]
    pub fn title(&self) -> &'static str {
        use DataPlaneError::{
    AuthFailed, CircuitBreakerOpen, ConnectionTimeout, CorsMethodNotAllowed,
    CorsOriginNotAllowed, Downstream, Forbidden, IdleTimeout, Internal,
    InvalidTargetHost, LinkUnavailable, MissingTargetHost, PayloadTooLarge, PluginNotFound,
    Protocol, RateLimitExceeded, RequestTimeout, RouteNotFound, SecretNotFound,
    StreamAborted, UnknownTargetHost, Validation,
};
        match self {
            Validation { .. } => "Validation Error",
            MissingTargetHost { .. } => "Missing Target Host",
            InvalidTargetHost { .. } => "Invalid Target Host",
            UnknownTargetHost { .. } => "Unknown Target Host",
            AuthFailed { .. } => "Authentication Failed",
            Forbidden { .. } => "Permission Denied",
            RouteNotFound { .. } => "Route Not Found",
            PluginNotFound { .. } => "Plugin Not Found",
            PayloadTooLarge { .. } => "Payload Too Large",
            RateLimitExceeded { .. } => "Rate Limit Exceeded",
            SecretNotFound { .. } => "Secret Not Found",
            Protocol { .. } => "Protocol Error",
            Downstream { .. } => "Downstream Error",
            StreamAborted { .. } => "Stream Aborted",
            LinkUnavailable { .. } => "Link Unavailable",
            CircuitBreakerOpen { .. } => "Circuit Breaker Open",
            ConnectionTimeout { .. } => "Connection Timeout",
            RequestTimeout { .. } => "Request Timeout",
            IdleTimeout { .. } => "Idle Timeout",
            CorsOriginNotAllowed { .. } => "CORS Origin Not Allowed",
            CorsMethodNotAllowed { .. } => "CORS Method Not Allowed",
            Internal { .. } => "Internal",
        }
    }

    /// RFC 9457 extension fields for this error.
    #[must_use]
    pub fn extensions(&self) -> &ErrorExtensions {
        use DataPlaneError::{
    AuthFailed, CircuitBreakerOpen, ConnectionTimeout, CorsMethodNotAllowed,
    CorsOriginNotAllowed, Downstream, Forbidden, IdleTimeout, Internal,
    InvalidTargetHost, LinkUnavailable, MissingTargetHost, PayloadTooLarge, PluginNotFound,
    Protocol, RateLimitExceeded, RequestTimeout, RouteNotFound, SecretNotFound,
    StreamAborted, UnknownTargetHost, Validation,
};
        match self {
            Validation { extensions, .. }
            | MissingTargetHost { extensions, .. }
            | InvalidTargetHost { extensions, .. }
            | UnknownTargetHost { extensions, .. }
            | AuthFailed { extensions, .. }
            | Forbidden { extensions, .. }
            | RouteNotFound { extensions, .. }
            | PluginNotFound { extensions, .. }
            | PayloadTooLarge { extensions, .. }
            | SecretNotFound { extensions, .. }
            | Protocol { extensions, .. }
            | Downstream { extensions, .. }
            | StreamAborted { extensions, .. }
            | LinkUnavailable { extensions, .. }
            | CircuitBreakerOpen { extensions }
            | ConnectionTimeout { extensions, .. }
            | RequestTimeout { extensions, .. }
            | IdleTimeout { extensions, .. }
            | CorsOriginNotAllowed { extensions, .. }
            | CorsMethodNotAllowed { extensions, .. }
            | Internal { extensions, .. }
            | RateLimitExceeded { extensions, .. } => extensions,
        }
    }

    /// Build a generic `Internal` variant carrying a caller-set extensions
    /// object.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
            extensions: ErrorExtensions::default(),
        }
    }

    /// Build a PEP-denial `Forbidden` variant carrying a caller-set
    /// extensions object (HTTP 403).
    #[must_use]
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::Forbidden {
            detail: detail.into(),
            extensions: ErrorExtensions::default(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    type StatusCase<'a> = (fn() -> DataPlaneError, u16, &'a str);

    #[test]
    fn status_and_type_matrix_matches_docs() {
        let cases: &[StatusCase] = &[
            (
                || DataPlaneError::Validation {
                    detail: "x".into(),
                    extensions: ErrorExtensions::default(),
                },
                400,
                gts_helpers::ERR_VALIDATION,
            ),
            (
                || DataPlaneError::MissingTargetHost {
                    valid_hosts: vec!["a".into()],
                    extensions: ErrorExtensions::default(),
                },
                400,
                gts_helpers::ERR_MISSING_TARGET_HOST,
            ),
            (
                || DataPlaneError::RouteNotFound {
                    detail: "x".into(),
                    extensions: ErrorExtensions::default(),
                },
                404,
                gts_helpers::ERR_ROUTE_NOT_FOUND,
            ),
            (
                || DataPlaneError::RateLimitExceeded {
                    retry_after_seconds: 30,
                    extensions: ErrorExtensions::default(),
                },
                429,
                gts_helpers::ERR_RATE_LIMIT_EXCEEDED,
            ),
            (
                || DataPlaneError::ConnectionTimeout {
                    detail: "x".into(),
                    extensions: ErrorExtensions::default(),
                },
                504,
                gts_helpers::ERR_TIMEOUT_CONNECTION,
            ),
            (
                || DataPlaneError::Forbidden {
                    detail: "x".into(),
                    extensions: ErrorExtensions::default(),
                },
                403,
                gts_helpers::ERR_PERMISSION_DENIED,
            ),
        ];
        for (make, status, id) in cases {
            let err = make();
            assert_eq!(err.status(), *status, "{err:?}");
            assert_eq!(err.error_type(), *id, "{err:?}");
        }
    }

    #[test]
    fn retryability_matches_docs() {
        assert!(DataPlaneError::RateLimitExceeded {
            retry_after_seconds: 1,
            extensions: ErrorExtensions::default()
        }
        .is_retryable());
        assert!(!DataPlaneError::Validation {
            detail: "x".into(),
            extensions: ErrorExtensions::default()
        }
        .is_retryable());
    }
}
