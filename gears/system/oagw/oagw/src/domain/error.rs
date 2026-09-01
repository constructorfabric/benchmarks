//! Domain error taxonomy for the OAGW gear.
//!
//! Every variant maps to exactly one RFC 9457 problem on the wire. The
//! transport-side mapping lives in [`crate::api::rest::error`], which reads
//! [`OagwErrorType`] for the GTS `type` each OAGW-specific variant projects to.

use std::time::Duration;

use thiserror::Error;
use toolkit_gts::gts_id;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The OAGW-specific error identities mandated by `DESIGN` §3.3 (Error
/// Response Format). Each carries the GTS instance id the wire body's `type`
/// field carries, verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OagwErrorType {
    /// `cf.oagw.validation.error.v1`
    Validation,
    /// `cf.oagw.routing.missing_target_host.v1`
    MissingTargetHost,
    /// `cf.oagw.routing.invalid_target_host.v1`
    InvalidTargetHost,
    /// `cf.oagw.routing.unknown_target_host.v1`
    UnknownTargetHost,
    /// `cf.oagw.auth.failed.v1`
    AuthFailed,
    /// `cf.oagw.route.not_found.v1`
    RouteNotFound,
    /// `cf.oagw.plugin.in_use.v1`
    PluginInUse,
    /// `cf.oagw.payload.too_large.v1`
    PayloadTooLarge,
    /// `cf.oagw.rate_limit.exceeded.v1`
    RateLimitExceeded,
    /// `cf.oagw.secret.not_found.v1`
    SecretNotFound,
    /// `cf.oagw.link.unavailable.v1`
    LinkUnavailable,
    /// `cf.oagw.protocol.error.v1`
    ProtocolError,
    /// `cf.oagw.downstream.error.v1`
    DownstreamError,
    /// `cf.oagw.stream.aborted.v1`
    StreamAborted,
    /// `cf.oagw.circuit_breaker.open.v1`
    CircuitBreakerOpen,
    /// `cf.oagw.plugin.not_found.v1`
    PluginNotFound,
    /// `cf.oagw.timeout.connection.v1`
    ConnectionTimeout,
    /// `cf.oagw.timeout.request.v1`
    RequestTimeout,
    /// `cf.oagw.timeout.idle.v1`
    IdleTimeout,
}

impl OagwErrorType {
    /// GTS instance id for this error, exactly as tabulated in `DESIGN` §3.3.
    #[must_use]
    pub fn gts_id(self) -> &'static str {
        match self {
            Self::Validation => gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1"),
            Self::MissingTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1")
            }
            Self::InvalidTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1")
            }
            Self::UnknownTargetHost => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1")
            }
            Self::AuthFailed => gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1"),
            Self::RouteNotFound => gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1"),
            Self::PluginInUse => gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"),
            Self::PayloadTooLarge => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1")
            }
            Self::RateLimitExceeded => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
            }
            Self::SecretNotFound => gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"),
            Self::LinkUnavailable => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1")
            }
            Self::ProtocolError => gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1"),
            Self::DownstreamError => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1")
            }
            Self::StreamAborted => gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"),
            Self::CircuitBreakerOpen => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1")
            }
            Self::PluginNotFound => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1")
            }
            Self::ConnectionTimeout => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1")
            }
            Self::RequestTimeout => gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1"),
            Self::IdleTimeout => gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"),
        }
    }

    /// Human-readable RFC 9457 `title` for this error identity.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Validation => "Validation Error",
            Self::MissingTargetHost => "Missing Target Host Header",
            Self::InvalidTargetHost => "Invalid Target Host Format",
            Self::UnknownTargetHost => "Unknown Target Host",
            Self::AuthFailed => "Authentication Failed",
            Self::RouteNotFound => "Route Not Found",
            Self::PluginInUse => "Plugin In Use",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::RateLimitExceeded => "Rate Limit Exceeded",
            Self::SecretNotFound => "Secret Not Found",
            Self::LinkUnavailable => "Link Unavailable",
            Self::ProtocolError => "Protocol Error",
            Self::DownstreamError => "Downstream Error",
            Self::StreamAborted => "Stream Aborted",
            Self::CircuitBreakerOpen => "Circuit Breaker Open",
            Self::PluginNotFound => "Plugin Not Found",
            Self::ConnectionTimeout => "Connection Timeout",
            Self::RequestTimeout => "Request Timeout",
            Self::IdleTimeout => "Idle Timeout",
        }
    }

    /// HTTP status this identity maps to.
    #[must_use]
    pub const fn status(self) -> u16 {
        match self {
            Self::Validation
            | Self::MissingTargetHost
            | Self::InvalidTargetHost
            | Self::UnknownTargetHost => 400,
            Self::AuthFailed => 401,
            Self::RouteNotFound => 404,
            Self::PluginInUse => 409,
            Self::PayloadTooLarge => 413,
            Self::RateLimitExceeded => 429,
            Self::SecretNotFound => 500,
            Self::LinkUnavailable | Self::CircuitBreakerOpen | Self::PluginNotFound => 503,
            Self::ProtocolError | Self::DownstreamError | Self::StreamAborted => 502,
            Self::ConnectionTimeout | Self::RequestTimeout | Self::IdleTimeout => 504,
        }
    }

    /// `true` when the error is only reachable on the proxy path. Management
    /// handlers never produce these identities, so a `type` seen on a
    /// management route always means a request-side mistake.
    #[must_use]
    pub const fn is_proxy_error(self) -> bool {
        matches!(
            self,
            Self::MissingTargetHost
                | Self::InvalidTargetHost
                | Self::UnknownTargetHost
                | Self::AuthFailed
                | Self::RouteNotFound
                | Self::PayloadTooLarge
                | Self::RateLimitExceeded
                | Self::SecretNotFound
                | Self::LinkUnavailable
                | Self::ProtocolError
                | Self::DownstreamError
                | Self::StreamAborted
                | Self::CircuitBreakerOpen
                | Self::PluginNotFound
                | Self::ConnectionTimeout
                | Self::RequestTimeout
                | Self::IdleTimeout
        )
    }
}

/// Domain error taxonomy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DomainError {
    /// Request or payload validation failed (`400`,
    /// `cf.oagw.validation.error.v1`).
    #[error("{detail}")]
    Validation { detail: String },

    /// `X-OAGW-Target-Host` is required to disambiguate a multi-endpoint
    /// upstream whose alias is a shared registrable suffix (`400`).
    #[error(
        "X-OAGW-Target-Host header required for multi-endpoint upstream with \
         common suffix alias"
    )]
    MissingTargetHost {
        alias: String,
        valid_hosts: Vec<String>,
    },

    /// `X-OAGW-Target-Host` is not a bare hostname or IP (`400`).
    #[error(
        "X-OAGW-Target-Host header format is invalid (must be hostname or IP, \
         no port/path/special chars)"
    )]
    InvalidTargetHost { invalid_value: String },

    /// `X-OAGW-Target-Host` names no configured endpoint (`400`).
    #[error("X-OAGW-Target-Host value does not match any configured endpoint")]
    UnknownTargetHost {
        invalid_value: String,
        valid_hosts: Vec<String>,
    },

    /// Credential injection against the upstream failed (`401`).
    #[error("{detail}")]
    AuthenticationFailed { detail: String },

    /// No route matched the request (`404`, data plane).
    #[error("no matching route found")]
    RouteNotFound { alias: String, path: String },

    /// The plugin is still referenced by an upstream or a route (`409`).
    #[error("plugin is referenced by an upstream or a route")]
    PluginInUse {
        /// GTS id of the plugin, echoed back in the problem body.
        plugin_id: String,
        /// Upstream GTS ids still binding the plugin.
        upstreams: Vec<String>,
        /// Route GTS ids still binding the plugin.
        routes: Vec<String>,
    },

    /// Payload exceeds the hard body limit (`413`).
    #[error("request payload exceeds limit of {limit_bytes} bytes")]
    PayloadTooLarge { limit_bytes: u64 },

    /// Rate limit exhausted (`429`, later slice).
    #[error("{detail}")]
    RateLimitExceeded {
        detail: String,
        retry_after: Option<Duration>,
    },

    /// The referenced `cred_store` secret is not resolvable (`500`).
    #[error("{detail}")]
    SecretNotFound { detail: String },

    /// Upstream link unavailable (`503`). Also the data-plane placeholder.
    #[error("{detail}")]
    LinkUnavailable {
        detail: String,
        retry_after: Option<Duration>,
    },

    /// The upstream spoke a protocol the gateway could not forward (`502`).
    #[error("{detail}")]
    ProtocolError {
        detail: String,
        /// Correlation id of the proxy request, for the wire extension.
        trace_id: Option<String>,
    },

    /// The upstream answered in a way that cannot be forwarded (`502`).
    #[error("{detail}")]
    DownstreamError { detail: String },

    /// A proxied byte stream was aborted before it completed (`502`).
    #[error("{detail}")]
    StreamAborted { detail: String },

    /// The upstream circuit breaker is open (`503`, retriable).
    #[error("circuit breaker for upstream '{alias}' is open")]
    CircuitBreakerOpen {
        alias: String,
        /// Seconds until the breaker half-opens again.
        retry_after: u64,
    },

    /// A configured `plugin_ref` resolves to no registered plugin (`503`).
    #[error("plugin not found: {plugin_id}")]
    PluginNotFound { plugin_id: String },

    /// Establishing the upstream connection took too long (`504`).
    #[error("connection to upstream '{alias}' timed out after {limit_secs}s")]
    ConnectionTimeout {
        alias: String,
        /// Configured connect budget, in seconds.
        limit_secs: u64,
    },

    /// The whole upstream exchange exceeded its budget (`504`).
    #[error("upstream request timed out after {limit_secs}s")]
    RequestTimeout {
        /// Configured `proxy_timeout_secs`.
        limit_secs: u64,
    },

    /// A streamed exchange went idle past its budget (`504`).
    #[error("upstream stream idle past {limit_secs}s")]
    IdleTimeout {
        /// Configured idle budget, in seconds.
        limit_secs: u64,
    },

    /// Management resource does not exist in the calling tenant (`404`).
    #[error("{resource} not found")]
    NotFound { resource: String },

    /// Uniqueness or state conflict other than a plugin reference (`409`).
    #[error("{detail}")]
    Conflict { detail: String },

    /// The caller lacks the management permission for the operation (`403`).
    #[error("{detail}")]
    AccessDenied { detail: String },

    /// A query parameter is malformed or outside its accepted range (`400`).
    #[error("{detail}")]
    InvalidArgument { detail: String },

    /// A dependency is unavailable (`503`, fail-closed).
    #[error("{detail}")]
    ServiceUnavailable {
        detail: String,
        cause: Option<BoxError>,
    },

    /// Unexpected internal failure (`500`).
    #[error("internal error")]
    Internal {
        diagnostic: String,
        cause: Option<BoxError>,
    },
}

impl DomainError {
    /// Build a [`DomainError::Validation`] from anything string-like.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::Validation {
            detail: detail.into(),
        }
    }

    /// Build a [`DomainError::NotFound`] for a resource id.
    #[must_use]
    pub fn not_found(resource: String) -> Self {
        Self::NotFound { resource }
    }

    /// Build a [`DomainError::Internal`] with a boxed cause.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>, cause: impl Into<BoxError>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
            cause: Some(cause.into()),
        }
    }

    /// Build a [`DomainError::ServiceUnavailable`] with a boxed cause.
    #[must_use]
    pub fn unavailable(detail: impl Into<String>, cause: impl Into<BoxError>) -> Self {
        Self::ServiceUnavailable {
            detail: detail.into(),
            cause: Some(cause.into()),
        }
    }

    /// Build a [`DomainError::ProtocolError`] for a proxy request.
    #[must_use]
    pub fn protocol_error(detail: impl Into<String>, trace_id: Option<String>) -> Self {
        Self::ProtocolError {
            detail: detail.into(),
            trace_id,
        }
    }

    /// Build a [`DomainError::DownstreamError`].
    #[must_use]
    pub fn downstream_error(detail: impl Into<String>) -> Self {
        Self::DownstreamError {
            detail: detail.into(),
        }
    }

    /// Build a [`DomainError::StreamAborted`].
    #[must_use]
    pub fn stream_aborted(detail: impl Into<String>) -> Self {
        Self::StreamAborted {
            detail: detail.into(),
        }
    }

    /// Build a [`DomainError::CircuitBreakerOpen`] for an upstream.
    #[must_use]
    pub fn circuit_breaker_open(alias: impl Into<String>, retry_after: u64) -> Self {
        Self::CircuitBreakerOpen {
            alias: alias.into(),
            retry_after,
        }
    }

    /// Build a [`DomainError::PluginNotFound`] for a plugin GTS id.
    #[must_use]
    pub fn plugin_not_found(plugin_id: impl Into<String>) -> Self {
        Self::PluginNotFound {
            plugin_id: plugin_id.into(),
        }
    }

    /// Build a [`DomainError::ConnectionTimeout`] for an upstream.
    #[must_use]
    pub fn connection_timeout(alias: impl Into<String>, limit_secs: u64) -> Self {
        Self::ConnectionTimeout {
            alias: alias.into(),
            limit_secs,
        }
    }

    /// Build a [`DomainError::RequestTimeout`].
    #[must_use]
    pub fn request_timeout(limit_secs: u64) -> Self {
        Self::RequestTimeout { limit_secs }
    }

    /// Build a [`DomainError::IdleTimeout`].
    #[must_use]
    pub fn idle_timeout(limit_secs: u64) -> Self {
        Self::IdleTimeout { limit_secs }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "error_tests.rs"]
mod tests;
