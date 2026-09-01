//! OAGW error catalogue (DESIGN §3.3 "Error Response Format").
//!
//! Every variant maps 1:1 onto a row of the DESIGN error table and carries the
//! GTS instance id spelled `gts.cf.core.errors.err.v1~cf.oagw.<name>.v1`. The
//! transport mapping (RFC 9457 `application/problem+json` plus the
//! `X-OAGW-Error-Source` header) lives in `crate::api::rest::error`; this
//! module stays transport-free so the catalogue can be reused by the data
//! plane.
//!
//! Management-only errors (upstream/route not found, alias conflict, route
//! match conflict) are not listed in the data-plane table; they reuse the same
//! naming scheme (`cf.oagw.<resource>.not_found.v1`) so that clients can treat
//! the catalogue uniformly.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// GTS prefix shared by every OAGW error type.
const ERR_PREFIX: &str = "cf.oagw.";

/// Resources that can be reported as missing by the management API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// `gts.cf.core.oagw.upstream.v1~`
    Upstream,
    /// `gts.cf.core.oagw.route.v1~`
    Route,
    /// `gts.cf.core.oagw.{type}_plugin.v1~`
    Plugin,
}

impl ResourceKind {
    /// Lower-case resource name used inside GTS ids and problem `detail`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Route => "route",
            Self::Plugin => "plugin",
        }
    }

    /// Human-readable resource name used as the problem `title`.
    #[must_use]
    pub fn title_word(self) -> &'static str {
        match self {
            Self::Upstream => "Upstream",
            Self::Route => "Route",
            Self::Plugin => "Plugin",
        }
    }
}

impl std::fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Resources that currently reference a plugin (ADR 0001 `PluginInUse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct ReferencedBy {
    /// Upstreams binding the plugin.
    #[serde(default)]
    pub upstreams: Vec<String>,
    /// Routes binding the plugin.
    #[serde(default)]
    pub routes: Vec<String>,
}

impl ReferencedBy {
    /// Empty reference set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            upstreams: Vec::new(),
            routes: Vec::new(),
        }
    }

    /// Whether nothing references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

impl Default for ReferencedBy {
    fn default() -> Self {
        Self::empty()
    }
}

/// Static catalogue metadata for one error type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMeta {
    /// HTTP status code.
    pub status: u16,
    /// GTS type identifier (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`).
    pub gts_type: &'static str,
    /// RFC 9457 `title`.
    pub title: &'static str,
    /// Whether the caller may retry the request.
    pub retriable: bool,
}

/// Which CORS rule rejected a cross-origin request (ADR 0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorsRejection {
    /// The `Origin` is not listed in `cors.allowed_origins`.
    Origin,
    /// The request method is not listed in `cors.allowed_methods`.
    Method,
}

/// OAGW domain error: the single error type crossing the domain boundary.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DomainError {
    /// 400 — request validation failed.
    #[error("validation failed: {detail}")]
    ValidationError {
        /// Human-readable explanation.
        detail: String,
        /// Offending value, when a single field is at fault.
        invalid_value: Option<String>,
        /// Alias context, when the failure concerns alias derivation.
        alias: Option<String>,
    },
    /// 400 — alias could not be derived from the endpoint pool.
    #[error("alias not derivable: {detail}")]
    AliasNotDerivable {
        /// Human-readable explanation.
        detail: String,
        /// Normalised hosts considered during derivation.
        valid_hosts: Vec<String>,
    },
    /// 400 — the supplied alias differs from the derived one.
    #[error("alias mismatch: {detail}")]
    AliasMismatch {
        /// Human-readable explanation (includes the derived alias).
        detail: String,
        /// Operator-supplied alias.
        provided: String,
        /// Alias derived from the endpoints.
        derived: String,
    },
    /// 409 — `(tenant_id, alias)` already taken.
    #[error("alias conflict: {alias}")]
    AliasConflict {
        /// Conflicting alias.
        alias: String,
        /// Identifier of the upstream already holding the alias.
        existing_upstream_id: Uuid,
    },
    /// 409 — generic conflict (route match uniqueness, plugin name clash).
    #[error("conflict: {detail}")]
    Conflict {
        /// Human-readable explanation.
        detail: String,
        /// Offending value, when a single field is at fault.
        invalid_value: Option<String>,
    },
    /// 404 — referenced resource does not exist in the calling tenant.
    #[error("{resource} not found: {id}")]
    NotFound {
        /// Missing resource kind.
        resource: ResourceKind,
        /// Identifier as requested.
        id: String,
        /// Human-readable explanation.
        detail: String,
    },
    /// 409 — plugin deletion refused because references exist.
    #[error("plugin in use")]
    PluginInUse {
        /// Plugin identifier (full GTS id).
        plugin_id: String,
        /// Referencing resources.
        referenced_by: ReferencedBy,
    },
    /// 400 — upstream configuration rejected by the domain rules.
    #[error("route rejected: {detail}")]
    RouteError {
        /// Human-readable explanation.
        detail: String,
        /// Offending value.
        invalid_value: Option<String>,
    },
    // --- data-plane catalogue (DESIGN §3.3) -------------------------------
    /// 400 — `X-OAGW-Target-Host` missing for a pooled upstream.
    #[error("missing target host header")]
    MissingTargetHost {
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Request path.
        path: Option<String>,
        /// Normalised hosts of the pool.
        valid_hosts: Vec<String>,
    },
    /// 400 — `X-OAGW-Target-Host` is malformed.
    #[error("invalid target host")]
    InvalidTargetHost {
        /// Offending header value.
        invalid_value: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
    },
    /// 400 — `X-OAGW-Target-Host` matches no configured endpoint.
    #[error("unknown target host")]
    UnknownTargetHost {
        /// Offending header value.
        invalid_value: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Normalised hosts of the pool.
        valid_hosts: Vec<String>,
    },
    /// 401 — upstream authentication failed.
    #[error("upstream authentication failed: {detail}")]
    AuthenticationFailed {
        /// Human-readable explanation.
        detail: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
    },
    /// 404 — no route matched the request.
    #[error("route not found")]
    RouteNotFound {
        /// Request path.
        path: Option<String>,
    },
    /// 403 — a cross-origin request was rejected by the CORS policy (ADR 0004).
    #[error("cors request rejected: {detail}")]
    CorsForbidden {
        /// Which CORS rule rejected the request.
        reason: CorsRejection,
        /// Human-readable explanation.
        detail: String,
        /// Offending origin, when the request carried one.
        origin: Option<String>,
    },
    /// 413 — request body exceeds `max_payload_bytes`.
    #[error("payload too large")]
    PayloadTooLarge {
        /// Configured limit in bytes.
        limit_bytes: u64,
    },
    /// 429 — rate limit exhausted.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Seconds to wait before retrying.
        retry_after_seconds: u64,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
        /// Request path.
        path: Option<String>,
    },
    /// 500 — a referenced secret could not be resolved.
    #[error("secret not found: {detail}")]
    SecretNotFound {
        /// Human-readable explanation.
        detail: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
    },
    /// 502 — protocol-level failure while talking to the upstream.
    #[error("protocol error: {detail}")]
    ProtocolError {
        /// Human-readable explanation.
        detail: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
    },
    /// 502 — the upstream answered with an error (passthrough).
    #[error("downstream error: {detail}")]
    DownstreamError {
        /// Human-readable explanation.
        detail: String,
        /// Status returned by the upstream.
        upstream_status: u16,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
    },
    /// 502 — a streamed response was interrupted.
    #[error("stream aborted: {detail}")]
    StreamAborted {
        /// Human-readable explanation.
        detail: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
    },
    /// 503 — no healthy upstream link.
    #[error("upstream link unavailable")]
    LinkUnavailable {
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
    },
    /// 503 — circuit breaker is open.
    #[error("circuit breaker open")]
    CircuitBreakerOpen {
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
    },
    /// 503 — a bound plugin could not be resolved by the data plane.
    #[error("plugin not found: {detail}")]
    PluginNotFound {
        /// Plugin identifier.
        plugin_id: String,
        /// Human-readable explanation.
        detail: String,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
    },
    /// 504 — connection establishment timed out.
    #[error("connection timeout")]
    ConnectionTimeout {
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
        /// Upstream host.
        host: Option<String>,
    },
    /// 504 — full request budget exhausted.
    #[error("request timeout")]
    RequestTimeout {
        /// Configured budget in seconds.
        timeout_seconds: u64,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
    },
    /// 504 — streaming idle timeout.
    #[error("idle timeout")]
    IdleTimeout {
        /// Configured idle budget in seconds.
        timeout_seconds: u64,
        /// Upstream identifier.
        upstream_id: Option<Uuid>,
    },
    /// 500 — an unexpected internal failure (S5+ seam).
    #[error("internal error: {detail}")]
    Internal {
        /// Human-readable explanation (never leaks internals to the wire).
        detail: String,
    },
}

impl DomainError {
    /// Builds a 400 validation error without an offending value.
    #[must_use]
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::ValidationError {
            detail: detail.into(),
            invalid_value: None,
            alias: None,
        }
    }

    /// Builds a 400 validation error carrying the offending value.
    #[must_use]
    pub fn validation_with_value(detail: impl Into<String>, invalid_value: impl Into<String>) -> Self {
        Self::ValidationError {
            detail: detail.into(),
            invalid_value: Some(invalid_value.into()),
            alias: None,
        }
    }

    /// Builds a 404 not-found error for a management resource.
    #[must_use]
    pub fn not_found(resource: ResourceKind, id: &str) -> Self {
        Self::NotFound {
            resource,
            id: id.to_owned(),
            detail: format!(
                "no {} with id '{}' is visible to the calling tenant",
                resource.as_str(),
                id
            ),
        }
    }

    /// Catalogue metadata for this error.
    #[must_use]
    pub fn meta(&self) -> ErrorMeta {
        match self {
            Self::ValidationError { .. } => ErrorMeta {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                title: "Validation Error",
                retriable: false,
            },
            Self::AliasNotDerivable { .. } | Self::AliasMismatch { .. } => ErrorMeta {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                title: "Alias Validation Error",
                retriable: false,
            },
            Self::MissingTargetHost { .. } => ErrorMeta {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
                title: "Missing Target Host Header",
                retriable: false,
            },
            Self::InvalidTargetHost { .. } => ErrorMeta {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
                title: "Invalid Target Host Format",
                retriable: false,
            },
            Self::UnknownTargetHost { .. } => ErrorMeta {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
                title: "Unknown Target Host",
                retriable: false,
            },
            Self::RouteError { .. } => ErrorMeta {
                status: 400,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
                title: "Route Error",
                retriable: false,
            },
            Self::AuthenticationFailed { .. } => ErrorMeta {
                status: 401,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
                title: "Authentication Failed",
                retriable: false,
            },
            Self::CorsForbidden { reason, .. } => ErrorMeta {
                status: 403,
                gts_type: match reason {
                    CorsRejection::Origin => {
                        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1"
                    }
                    CorsRejection::Method => {
                        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1"
                    }
                },
                title: match reason {
                    CorsRejection::Origin => "CORS Origin Not Allowed",
                    CorsRejection::Method => "CORS Method Not Allowed",
                },
                retriable: false,
            },
            Self::RouteNotFound { .. } => ErrorMeta {
                status: 404,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
                title: "Route Not Found",
                retriable: false,
            },
            Self::NotFound { resource, .. } => ErrorMeta {
                status: 404,
                gts_type: match resource {
                    ResourceKind::Upstream => "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1",
                    ResourceKind::Route => "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
                    ResourceKind::Plugin => "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
                },
                title: match resource {
                    ResourceKind::Upstream => "Upstream Not Found",
                    ResourceKind::Route => "Route Not Found",
                    ResourceKind::Plugin => "Plugin Not Found",
                },
                retriable: false,
            },
            Self::AliasConflict { .. } => ErrorMeta {
                status: 409,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.alias.conflict.v1",
                title: "Alias Conflict",
                retriable: false,
            },
            Self::Conflict { .. } => ErrorMeta {
                status: 409,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.conflict.v1",
                title: "Conflict",
                retriable: false,
            },
            Self::PluginInUse { .. } => ErrorMeta {
                status: 409,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
                title: "Plugin In Use",
                retriable: false,
            },
            Self::PayloadTooLarge { .. } => ErrorMeta {
                status: 413,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
                title: "Payload Too Large",
                retriable: false,
            },
            Self::RateLimitExceeded { .. } => ErrorMeta {
                status: 429,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
                title: "Rate Limit Exceeded",
                retriable: true,
            },
            Self::SecretNotFound { .. } => ErrorMeta {
                status: 500,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
                title: "Secret Not Found",
                retriable: false,
            },
            Self::ProtocolError { .. } => ErrorMeta {
                status: 502,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
                title: "Protocol Error",
                retriable: false,
            },
            Self::DownstreamError { .. } => ErrorMeta {
                status: 502,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
                title: "Downstream Error",
                retriable: false,
            },
            Self::StreamAborted { .. } => ErrorMeta {
                status: 502,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
                title: "Stream Aborted",
                retriable: false,
            },
            Self::LinkUnavailable { .. } => ErrorMeta {
                status: 503,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
                title: "Upstream Link Unavailable",
                retriable: true,
            },
            Self::CircuitBreakerOpen { .. } => ErrorMeta {
                status: 503,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
                title: "Circuit Breaker Open",
                retriable: true,
            },
            Self::PluginNotFound { .. } => ErrorMeta {
                status: 503,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
                title: "Plugin Not Found",
                retriable: false,
            },
            Self::ConnectionTimeout { .. } => ErrorMeta {
                status: 504,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
                title: "Connection Timeout",
                retriable: true,
            },
            Self::RequestTimeout { .. } => ErrorMeta {
                status: 504,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
                title: "Request Timeout",
                retriable: true,
            },
            Self::IdleTimeout { .. } => ErrorMeta {
                status: 504,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
                title: "Idle Timeout",
                retriable: true,
            },
            Self::Internal { .. } => ErrorMeta {
                status: 500,
                gts_type: "gts.cf.core.errors.err.v1~cf.oagw.internal.error.v1",
                title: "Internal Error",
                retriable: false,
            },
        }
    }

    /// HTTP status code of this error.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.meta().status
    }

    /// GTS type identifier of this error.
    #[must_use]
    pub fn gts_type(&self) -> &'static str {
        self.meta().gts_type
    }

    /// RFC 9457 `title`.
    #[must_use]
    pub fn title(&self) -> &'static str {
        self.meta().title
    }

    /// RFC 9457 `detail`.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::ValidationError { detail, .. }
            | Self::AliasNotDerivable { detail, .. }
            | Self::AliasMismatch { detail, .. }
            | Self::Conflict { detail, .. }
            | Self::NotFound { detail, .. }
            | Self::AuthenticationFailed { detail, .. }
            | Self::ProtocolError { detail, .. }
            | Self::DownstreamError { detail, .. }
            | Self::StreamAborted { detail, .. }
            | Self::SecretNotFound { detail, .. }
            | Self::PluginNotFound { detail, .. }
            | Self::Internal { detail }
            | Self::RouteError { detail, .. }
            | Self::CorsForbidden { detail, .. } => detail.clone(),
            Self::AliasConflict { alias, .. } => {
                format!("an upstream with alias '{alias}' already exists in this tenant")
            }
            Self::PluginInUse { referenced_by, .. } => {
                let upstreams = referenced_by.upstreams.len();
                let routes = referenced_by.routes.len();
                format!(
                    "plugin is still referenced by {upstreams} upstream(s) and {routes} route(s)"
                )
            }
            Self::MissingTargetHost { valid_hosts, .. } => format!(
                "X-OAGW-Target-Host header is required for this upstream pool; valid hosts: {}",
                valid_hosts.join(", ")
            ),
            Self::InvalidTargetHost { invalid_value, .. } => format!(
                "X-OAGW-Target-Host '{invalid_value}' is not a valid hostname or IP address"
            ),
            Self::UnknownTargetHost { invalid_value, valid_hosts, .. } => format!(
                "X-OAGW-Target-Host '{invalid_value}' does not match any configured endpoint; \
                 valid hosts: {}",
                valid_hosts.join(", ")
            ),
            Self::RouteNotFound { .. } => {
                "no route matched the requested method and path".to_owned()
            }
            Self::PayloadTooLarge { limit_bytes } => {
                format!("request payload exceeds the {limit_bytes} byte limit")
            }
            Self::RateLimitExceeded { retry_after_seconds, .. } => format!(
                "rate limit exceeded; retry after {retry_after_seconds} seconds"
            ),
            Self::LinkUnavailable { .. } => "no healthy upstream endpoint is available".to_owned(),
            Self::CircuitBreakerOpen { .. } => {
                "the circuit breaker for this upstream is open".to_owned()
            }
            Self::ConnectionTimeout { .. } => {
                "timed out while establishing the upstream connection".to_owned()
            }
            Self::RequestTimeout { timeout_seconds, .. } => {
                format!("upstream call exceeded the {timeout_seconds} second budget")
            }
            Self::IdleTimeout { timeout_seconds, .. } => {
                format!("stream idle for more than {timeout_seconds} seconds")
            }
        }
    }

    /// `Retry-After` guidance, when the error is retriable.
    #[must_use]
    pub fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            Self::RateLimitExceeded {
                retry_after_seconds,
                ..
            } => Some(*retry_after_seconds),
            _ => self
                .meta()
                .retriable
                .then(|| default_retry_after(self.status())),
        }
    }

    /// OAGW-specific extension fields carried by the problem document.
    #[must_use]
    pub fn extensions(&self) -> ProblemExtensions {
        match self {
            Self::ValidationError {
                invalid_value,
                alias,
                ..
            } => ProblemExtensions {
                invalid_value: invalid_value.clone(),
                alias: alias.clone(),
                ..ProblemExtensions::default()
            },
            Self::AliasNotDerivable { valid_hosts, .. } => ProblemExtensions {
                valid_hosts: Some(valid_hosts.clone()),
                ..ProblemExtensions::default()
            },
            Self::AliasMismatch { provided, derived, .. } => ProblemExtensions {
                invalid_value: Some(provided.clone()),
                alias: Some(derived.clone()),
                ..ProblemExtensions::default()
            },
            Self::AliasConflict {
                alias,
                existing_upstream_id,
            } => ProblemExtensions {
                upstream_id: Some(existing_upstream_id.to_string()),
                alias: Some(alias.clone()),
                ..ProblemExtensions::default()
            },
            Self::RouteError { invalid_value, .. } | Self::Conflict { invalid_value, .. } => {
                ProblemExtensions {
                    invalid_value: invalid_value.clone(),
                    ..ProblemExtensions::default()
                }
            }
            Self::NotFound { id, .. } => ProblemExtensions {
                invalid_value: Some(id.clone()),
                ..ProblemExtensions::default()
            },
            Self::PluginInUse {
                plugin_id,
                referenced_by,
            } => ProblemExtensions {
                plugin_id: Some(plugin_id.clone()),
                referenced_by: Some(referenced_by.clone()),
                ..ProblemExtensions::default()
            },
            Self::MissingTargetHost {
                upstream_id,
                path,
                valid_hosts,
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                path: path.clone(),
                valid_hosts: Some(valid_hosts.clone()),
                ..ProblemExtensions::default()
            },
            Self::InvalidTargetHost {
                invalid_value,
                upstream_id,
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                invalid_value: Some(invalid_value.clone()),
                ..ProblemExtensions::default()
            },
            Self::UnknownTargetHost {
                invalid_value,
                upstream_id,
                valid_hosts,
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                valid_hosts: Some(valid_hosts.clone()),
                invalid_value: Some(invalid_value.clone()),
                ..ProblemExtensions::default()
            },
            Self::AuthenticationFailed {
                upstream_id, host, ..
            }
            | Self::ProtocolError {
                upstream_id, host, ..
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                host: host.clone(),
                ..ProblemExtensions::default()
            },
            Self::DownstreamError {
                upstream_id, host, ..
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                host: host.clone(),
                ..ProblemExtensions::default()
            },
            Self::StreamAborted { upstream_id, .. } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                ..ProblemExtensions::default()
            },
            Self::CorsForbidden { origin, .. } => ProblemExtensions {
                invalid_value: origin.clone(),
                ..ProblemExtensions::default()
            },
            Self::RouteNotFound { path } => ProblemExtensions {
                path: path.clone(),
                ..ProblemExtensions::default()
            },
            Self::RateLimitExceeded {
                upstream_id,
                host,
                path,
                ..
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                host: host.clone(),
                path: path.clone(),
                retry_after_seconds: self.retry_after_seconds(),
                ..ProblemExtensions::default()
            },
            Self::SecretNotFound { upstream_id, .. } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                ..ProblemExtensions::default()
            },
            Self::LinkUnavailable { upstream_id, host }
            | Self::CircuitBreakerOpen { upstream_id, host }
            | Self::ConnectionTimeout { upstream_id, host } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                host: host.clone(),
                ..ProblemExtensions::default()
            },
            Self::PluginNotFound {
                plugin_id,
                upstream_id,
                ..
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                plugin_id: Some(plugin_id.clone()),
                ..ProblemExtensions::default()
            },
            Self::RequestTimeout {
                timeout_seconds,
                upstream_id,
            }
            | Self::IdleTimeout {
                timeout_seconds,
                upstream_id,
            } => ProblemExtensions {
                upstream_id: upstream_id.map(|id| id.to_string()),
                retry_after_seconds: Some(*timeout_seconds),
                ..ProblemExtensions::default()
            },
            // `PayloadTooLarge` and `Internal` carry no structured extension
            // members: the problem document still names them via `type`,
            // `detail` and `trace_id`.
            Self::PayloadTooLarge { .. } | Self::Internal { .. } => {
                ProblemExtensions::default()
            }
        }
    }
}

/// OAGW-specific RFC 9457 extension members, in catalogue order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProblemExtensions {
    /// Upstream involved in the failure.
    pub upstream_id: Option<String>,
    /// Upstream host involved in the failure.
    pub host: Option<String>,
    /// Request path involved in the failure.
    pub path: Option<String>,
    /// Retry guidance in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Distributed-trace correlation identifier.
    pub trace_id: Option<String>,
    /// Endpoints that would have satisfied the request.
    pub valid_hosts: Option<Vec<String>>,
    /// The rejected value.
    pub invalid_value: Option<String>,
    /// Alias involved in the failure.
    pub alias: Option<String>,
    /// Plugin involved in the failure.
    pub plugin_id: Option<String>,
    /// Resources still referencing a plugin.
    pub referenced_by: Option<ReferencedBy>,
}

impl ProblemExtensions {
    /// Iterates the set members as `(member, json-value)` pairs, so the
    /// transport layer can flatten them into the problem document.
    #[must_use]
    pub fn iter_json(&self) -> Vec<(String, serde_json::Value)> {
        let mut out = Vec::new();
        if let Some(v) = &self.upstream_id {
            out.push(("upstream_id".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = &self.host {
            out.push(("host".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = &self.path {
            out.push(("path".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = self.retry_after_seconds {
            out.push(("retry_after_seconds".to_owned(), serde_json::Value::from(v)));
        }
        if let Some(v) = &self.trace_id {
            out.push(("trace_id".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = &self.valid_hosts {
            out.push(("valid_hosts".to_owned(), serde_json::to_value(v).unwrap_or_default()));
        }
        if let Some(v) = &self.invalid_value {
            out.push(("invalid_value".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = &self.alias {
            out.push(("alias".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = &self.plugin_id {
            out.push(("plugin_id".to_owned(), serde_json::Value::from(v.clone())));
        }
        if let Some(v) = &self.referenced_by {
            out.push(("referenced_by".to_owned(), serde_json::to_value(v).unwrap_or_default()));
        }
        out
    }
}

/// Builds the GTS type id for an OAGW error name.
///
/// Retained as a helper so future catalogue additions stay on the canonical
/// prefix; `ERR_PREFIX` would otherwise be unused.
#[must_use]
pub fn oagw_error_type(name: &str) -> String {
    format!("gts.cf.core.errors.err.v1~{ERR_PREFIX}{name}.v1")
}

/// Conservative retry guidance used when a retriable error carries no explicit
/// `retry_after_seconds` of its own (DESIGN §3.3 "retriable" column).
#[must_use]
pub fn default_retry_after(status: u16) -> u64 {
    match status {
        429 => 1,
        // 5xx retriable classes: ask the caller to back off for a short,
        // bounded period rather than retrying in a tight loop.
        _ => 2,
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
