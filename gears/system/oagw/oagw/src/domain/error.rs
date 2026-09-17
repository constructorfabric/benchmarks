//! Domain error type for the `oagw` control plane and data plane.
//!
//! This is the *single* error vocabulary of the gear. The REST transport maps
//! it onto RFC 9457 problem documents (see
//! [`crate::api::rest::error`]); the platform's `CanonicalError` mapping is
//! derived from the same type so `canonical_error_middleware` keeps working.
//!
//! Every variant pins its own HTTP status, so a regression that demotes a
//! documented status to 500 is a compile-time-visible mapping change rather
//! than a silent one.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Which side produced an error: the gateway itself or the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway produced the failure (default for every control-plane error).
    Gateway,
    /// The upstream produced the failure (proxied error responses).
    Upstream,
}

impl ErrorSource {
    /// Wire value of the `X-OAGW-Error-Source` header.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// Plugins / routes that still reference a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginReferences {
    /// Upstream ids (GTS instance ids) whose upstream-level config references
    /// the plugin.
    pub upstreams: Vec<String>,
    /// Route ids (GTS instance ids) whose route-level config references the
    /// plugin.
    pub routes: Vec<String>,
}

impl PluginReferences {
    /// True when nothing references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }

    /// Total number of referencing resources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.upstreams.len() + self.routes.len()
    }
}

/// A single field-level validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldViolation {
    /// Dot-separated path of the offending field (e.g. `server.endpoints[0].port`).
    pub field: String,
    /// Human-readable explanation.
    pub detail: String,
}

impl FieldViolation {
    /// Shorthand constructor.
    #[must_use]
    pub fn new(field: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            detail: detail.into(),
        }
    }
}

/// Every error the OAGW gear can produce.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    // -- 400 -----------------------------------------------------------------
    /// Request body or query failed validation.
    #[error("validation failed: {detail}")]
    Validation {
        /// Collected per-field violations (may be empty for a whole-object error).
        violations: Vec<FieldViolation>,
        /// Human-readable summary.
        detail: String,
    },
    /// The upstream has no endpoint that can serve the resolved alias.
    #[error("upstream {upstream_id} has no endpoint for alias {alias:?}")]
    MissingTargetHost {
        /// Upstream that lacks a usable endpoint.
        upstream_id: Uuid,
        /// Alias that could not be satisfied.
        alias: String,
    },
    /// The alias does not name an endpoint of the upstream.
    #[error("alias {invalid_value:?} does not name an endpoint of upstream {upstream_id}")]
    InvalidTargetHost {
        /// Upstream that was resolved.
        upstream_id: Uuid,
        /// Offending alias.
        invalid_value: String,
        /// Hosts the upstream would have accepted, so the client can correct
        /// the header (ADR 0007 "X-OAGW-Target-Host Error Examples").
        valid_hosts: Vec<String>,
    },
    /// The `X-OAGW-Target-Host` value is well formed but matches no configured
    /// endpoint (DESIGN error table: `routing.unknown_target_host.v1`).
    #[error(
        "X-OAGW-Target-Host {invalid_value:?} does not match any configured endpoint of upstream {upstream_id}"
    )]
    UnknownTargetHost {
        /// Upstream that was resolved.
        upstream_id: Uuid,
        /// Offending header value.
        invalid_value: String,
        /// Configured hosts the header could have named (ADR 0007).
        valid_hosts: Vec<String>,
    },
    /// Alias would change on update (alias is immutable, ADR 0003).
    #[error("alias is immutable: {detail}")]
    AliasImmutable {
        /// Human-readable explanation.
        detail: String,
    },
    /// A route's `upstream_id` was changed on update.
    #[error("route upstream_id is immutable: {detail}")]
    UpstreamIdImmutable {
        /// Human-readable explanation.
        detail: String,
    },
    /// The request method is not served by any matching route (405).
    #[error("method {method} is not allowed for this route")]
    MethodNotAllowed {
        /// The rejected method.
        method: String,
        /// Comma-separated methods the matching routes serve.
        allow: String,
    },

    // -- 401 -----------------------------------------------------------------
    /// Credentials were missing or invalid.
    #[error("authentication failed: {detail}")]
    AuthenticationFailed {
        /// Human-readable explanation (never echoes credentials).
        detail: String,
    },

    // -- 404 -----------------------------------------------------------------
    /// No upstream with that id in this tenant.
    #[error("upstream {id} not found")]
    UpstreamNotFound {
        /// Requested upstream id.
        id: Uuid,
    },
    /// No route matched, or the referenced route does not exist.
    #[error("route not found: {detail}")]
    RouteNotFound {
        /// Human-readable explanation.
        detail: String,
    },
    /// No plugin with that id in this tenant.
    #[error("plugin {id} not found")]
    PluginNotFound {
        /// Requested plugin id.
        id: Uuid,
    },

    // -- 409 -----------------------------------------------------------------
    /// The alias is already taken inside this tenant.
    #[error("alias {alias:?} is already in use in this tenant")]
    AliasConflict {
        /// Colliding alias.
        alias: String,
    },
    /// Two routes in the same tenant resolve to the same `match`.
    #[error("route match conflict: {detail}")]
    RouteMatchConflict {
        /// Human-readable explanation.
        detail: String,
    },
    /// The plugin is still bound to upstreams/routes.
    #[error("plugin {plugin_id} is still referenced")]
    PluginInUse {
        /// GTS instance id of the plugin.
        plugin_id: String,
        /// Resources that still reference it.
        referenced_by: PluginReferences,
    },

    // -- 413 -----------------------------------------------------------------
    /// Request body exceeds the configured limit.
    #[error("request body exceeds the configured limit of {limit_bytes} bytes")]
    PayloadTooLarge {
        /// Configured limit in bytes.
        limit_bytes: u64,
    },

    // -- 429 -----------------------------------------------------------------
    /// Rate limit exhausted.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Suggested client retry delay.
        retry_after_seconds: u64,
    },

    // -- 500 -----------------------------------------------------------------
    /// A credstore secret backing a plugin could not be read.
    #[error("secret unavailable: {detail}")]
    SecretNotFound {
        /// Human-readable explanation.
        detail: String,
    },
    /// Unmapped server-side failure.
    #[error("internal error: {diagnostic}")]
    Internal {
        /// Diagnostic suitable for logs (never leaks secret material).
        diagnostic: String,
    },

    // -- 502 -----------------------------------------------------------------
    /// The upstream answered in a way the protocol layer cannot interpret.
    #[error("protocol error: {detail}")]
    ProtocolError {
        /// Human-readable explanation.
        detail: String,
    },
    /// The upstream service failed the exchange at the transport level: the
    /// connection was refused or reset, the host did not resolve, the TLS
    /// handshake failed, or the connection dropped before a usable response
    /// arrived (DESIGN error table: `downstream.error.v1`, "Upstream service
    /// error").
    ///
    /// The problem document is synthesised by the gateway, so ADR 0007 makes it
    /// a *gateway*-sourced error: `upstream` is reserved for a response the
    /// upstream actually produced and that is passed through unchanged.
    #[error("upstream error: {detail}")]
    DownstreamError {
        /// Human-readable explanation.
        detail: String,
        /// Upstream that was called.
        upstream_id: Option<Uuid>,
        /// Target host that was called.
        host: Option<String>,
        /// Request path that was sent upstream.
        path: Option<String>,
    },
    /// A streaming exchange aborted mid-flight.
    #[error("stream aborted: {detail}")]
    StreamAborted {
        /// Human-readable explanation.
        detail: String,
    },

    // -- 403 -----------------------------------------------------------------
    /// A cross-origin request whose origin is not in `allowed_origins`.
    #[error("CORS origin not allowed: {detail}")]
    CorsOriginNotAllowed {
        /// Human-readable explanation.
        detail: String,
    },
    /// A cross-origin request whose method is not in `allowed_methods`.
    #[error("CORS method not allowed: {detail}")]
    CorsMethodNotAllowed {
        /// Human-readable explanation.
        detail: String,
    },

    // -- 503 -----------------------------------------------------------------
    /// The upstream is administratively disabled or otherwise unreachable.
    #[error("link unavailable: {detail}")]
    LinkUnavailable {
        /// Human-readable explanation.
        detail: String,
        /// Suggested client retry delay, when known.
        retry_after_seconds: Option<u64>,
    },
    /// The upstream is administratively disabled.
    #[error("upstream {upstream_id} is disabled")]
    UpstreamDisabled {
        /// Disabled upstream.
        upstream_id: Uuid,
        /// Target host that would have been called.
        host: Option<String>,
    },
    /// A required plugin implementation is not loaded in this process.
    #[error("plugin implementation unavailable: {detail}")]
    PluginUnavailable {
        /// Human-readable explanation (names the plugin id).
        detail: String,
    },
    /// The circuit breaker for the upstream is open.
    #[error("circuit breaker open: {detail}")]
    CircuitBreakerOpen {
        /// Human-readable explanation.
        detail: String,
        /// Suggested client retry delay.
        retry_after_seconds: Option<u64>,
    },

    // -- 504 -----------------------------------------------------------------
    /// No connection could be established in time.
    #[error("connection timeout: {detail}")]
    ConnectionTimeout {
        /// Human-readable explanation.
        detail: String,
        /// Suggested client retry delay.
        retry_after_seconds: Option<u64>,
    },
    /// The full upstream response was not received in time.
    #[error("request timeout: {detail}")]
    RequestTimeout {
        /// Human-readable explanation.
        detail: String,
        /// Upstream that was called.
        upstream_id: Option<Uuid>,
        /// Target host that was called.
        host: Option<String>,
        /// Request path that was sent upstream.
        path: Option<String>,
        /// Suggested client retry delay.
        retry_after_seconds: Option<u64>,
    },
    /// A streaming exchange went idle past the idle timeout.
    #[error("idle timeout: {detail}")]
    IdleTimeout {
        /// Human-readable explanation.
        detail: String,
        /// Suggested client retry delay.
        retry_after_seconds: Option<u64>,
    },
}

impl DomainError {
    /// HTTP status this error maps to.
    #[must_use]
    pub fn status(&self) -> u16 {
        use DomainError as E;
        match self {
            E::Validation { .. }
            | E::MissingTargetHost { .. }
            | E::InvalidTargetHost { .. }
            | E::UnknownTargetHost { .. }
            | E::AliasImmutable { .. }
            | E::UpstreamIdImmutable { .. } => 400,
            E::MethodNotAllowed { .. } => 405,
            E::AuthenticationFailed { .. } => 401,
            E::CorsOriginNotAllowed { .. } | E::CorsMethodNotAllowed { .. } => 403,
            E::UpstreamNotFound { .. } | E::RouteNotFound { .. } | E::PluginNotFound { .. } => 404,
            E::AliasConflict { .. } | E::RouteMatchConflict { .. } | E::PluginInUse { .. } => 409,
            E::PayloadTooLarge { .. } => 413,
            E::RateLimitExceeded { .. } => 429,
            E::SecretNotFound { .. } | E::Internal { .. } => 500,
            E::ProtocolError { .. } | E::DownstreamError { .. } | E::StreamAborted { .. } => 502,
            E::LinkUnavailable { .. }
            | E::UpstreamDisabled { .. }
            | E::PluginUnavailable { .. }
            | E::CircuitBreakerOpen { .. } => 503,
            E::ConnectionTimeout { .. } | E::RequestTimeout { .. } | E::IdleTimeout { .. } => 504,
        }
    }

    /// OAGW problem-type id for this error (DESIGN.md error table).
    #[must_use]
    pub fn problem_type(&self) -> &'static str {
        use crate::domain::gts_helpers::problem as p;
        use DomainError as E;
        match self {
            E::Validation { .. } => p::VALIDATION_ERROR,
            E::MissingTargetHost { .. } => p::MISSING_TARGET_HOST,
            E::InvalidTargetHost { .. } => p::INVALID_TARGET_HOST,
            E::UnknownTargetHost { .. } => p::UNKNOWN_TARGET_HOST,
            E::AliasImmutable { .. } => p::ALIAS_IMMUTABLE,
            E::UpstreamIdImmutable { .. } => p::UPSTREAM_ID_IMMUTABLE,
            E::MethodNotAllowed { .. } => p::ROUTE_METHOD_NOT_ALLOWED,
            E::AuthenticationFailed { .. } => p::AUTHENTICATION_FAILED,
            E::UpstreamNotFound { .. } => p::UPSTREAM_NOT_FOUND,
            E::RouteNotFound { .. } => p::ROUTE_NOT_FOUND,
            E::PluginNotFound { .. } => p::PLUGIN_NOT_FOUND,
            E::AliasConflict { .. } => p::ALIAS_CONFLICT,
            E::RouteMatchConflict { .. } => p::ROUTE_MATCH_CONFLICT,
            E::PluginInUse { .. } => p::PLUGIN_IN_USE,
            E::PayloadTooLarge { .. } => p::PAYLOAD_TOO_LARGE,
            E::RateLimitExceeded { .. } => p::RATE_LIMIT_EXCEEDED,
            E::SecretNotFound { .. } => p::SECRET_NOT_FOUND,
            E::Internal { .. } => p::INTERNAL,
            E::ProtocolError { .. } => p::PROTOCOL_ERROR,
            E::DownstreamError { .. } => p::DOWNSTREAM_ERROR,
            E::StreamAborted { .. } => p::STREAM_ABORTED,
            E::CorsOriginNotAllowed { .. } => p::CORS_ORIGIN_NOT_ALLOWED,
            E::CorsMethodNotAllowed { .. } => p::CORS_METHOD_NOT_ALLOWED,
            E::LinkUnavailable { .. } => p::LINK_UNAVAILABLE,
            E::UpstreamDisabled { .. } => p::UPSTREAM_DISABLED,
            // The DESIGN error table names the proxy-side "plugin not loaded"
            // case `plugin.not_found.v1` with status 503.
            E::PluginUnavailable { .. } => p::PLUGIN_NOT_FOUND,
            E::CircuitBreakerOpen { .. } => p::CIRCUIT_BREAKER_OPEN,
            E::ConnectionTimeout { .. } => p::CONNECTION_TIMEOUT,
            E::RequestTimeout { .. } => p::REQUEST_TIMEOUT,
            E::IdleTimeout { .. } => p::IDLE_TIMEOUT,
        }
    }

    /// RFC 9457 `title` for this error's problem type.
    #[must_use]
    pub fn title(&self) -> &'static str {
        use DomainError as E;
        match self {
            E::Validation { .. }
            | E::MissingTargetHost { .. }
            | E::InvalidTargetHost { .. }
            | E::UnknownTargetHost { .. }
            | E::AliasImmutable { .. }
            | E::UpstreamIdImmutable { .. } => "Request validation failed",
            E::MethodNotAllowed { .. } => "Method not allowed",
            E::AuthenticationFailed { .. } => "Authentication failed",
            E::UpstreamNotFound { .. } => "Upstream not found",
            E::RouteNotFound { .. } => "Route not found",
            E::PluginNotFound { .. } => "Plugin not found",
            E::AliasConflict { .. } => "Alias already in use",
            E::RouteMatchConflict { .. } => "Route match conflict",
            E::PluginInUse { .. } => "Plugin still referenced",
            E::PayloadTooLarge { .. } => "Payload too large",
            E::RateLimitExceeded { .. } => "Rate limit exceeded",
            E::SecretNotFound { .. } => "Secret unavailable",
            E::Internal { .. } => "Internal error",
            E::ProtocolError { .. } => "Upstream protocol error",
            E::DownstreamError { .. } => "Upstream returned an error",
            E::StreamAborted { .. } => "Upstream stream aborted",
            E::CorsOriginNotAllowed { .. } => "CORS Origin Not Allowed",
            E::CorsMethodNotAllowed { .. } => "CORS Method Not Allowed",
            E::LinkUnavailable { .. } => "Upstream unavailable",
            E::UpstreamDisabled { .. } => "Upstream disabled",
            E::PluginUnavailable { .. } => "Plugin unavailable",
            E::CircuitBreakerOpen { .. } => "Circuit breaker open",
            E::ConnectionTimeout { .. } => "Connection timeout",
            E::RequestTimeout { .. } => "Request timeout",
            E::IdleTimeout { .. } => "Idle timeout",
        }
    }

    /// Whether the upstream (rather than the gateway) produced this error.
    ///
    /// ADR 0007: `upstream` marks a response the upstream *produced* and that is
    /// passed through unchanged; every problem document the gateway synthesises
    /// — including a `502` for an upstream that could not be reached — is
    /// `gateway`.
    #[must_use]
    pub fn source(&self) -> ErrorSource {
        use DomainError as E;
        match self {
            E::ProtocolError { .. } | E::StreamAborted { .. } => ErrorSource::Upstream,
            _ => ErrorSource::Gateway,
        }
    }

    /// Convenience constructor for a single-field validation failure.
    #[must_use]
    pub fn validation(field: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Validation {
            violations: vec![FieldViolation::new(field, detail)],
            detail: "request body failed validation".to_owned(),
        }
    }

    /// Convenience constructor for a whole-object validation failure.
    #[must_use]
    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::Validation {
            violations: Vec::new(),
            detail: detail.into(),
        }
    }

    /// Convenience constructor for an unmapped server-side failure.
    #[must_use]
    pub fn internal(diagnostic: impl Into<String>) -> Self {
        Self::Internal {
            diagnostic: diagnostic.into(),
        }
    }

    /// Retry delay attached to this error, when one is advertised.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        use DomainError as E;
        match self {
            E::RateLimitExceeded {
                retry_after_seconds,
            }
            | E::LinkUnavailable {
                retry_after_seconds: Some(retry_after_seconds),
                ..
            }
            | E::CircuitBreakerOpen {
                retry_after_seconds: Some(retry_after_seconds),
                ..
            }
            | E::ConnectionTimeout {
                retry_after_seconds: Some(retry_after_seconds),
                ..
            }
            | E::IdleTimeout {
                retry_after_seconds: Some(retry_after_seconds),
                ..
            }
            | E::RequestTimeout {
                retry_after_seconds: Some(retry_after_seconds),
                ..
            } => Some(Duration::from_secs(*retry_after_seconds)),
            _ => None,
        }
    }
}

impl From<std::fmt::Error> for DomainError {
    fn from(err: std::fmt::Error) -> Self {
        Self::Internal {
            diagnostic: format!("formatting failure: {err}"),
        }
    }
}
