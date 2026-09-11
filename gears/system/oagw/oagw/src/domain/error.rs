//! Domain errors for the OAGW gear.
//!
//! The enum covers exactly the error table fixed by the DESIGN's interface
//! contract (`cpt-cf-oagw-interface-api`): one variant per row, each carrying
//! its HTTP status, its GTS error type and its retriable flag. The enum carries
//! no HTTP type: the status is a `u16`, and the mapping to
//! `application/problem+json` lives in the single transport mapping layer
//! (`crate::api::rest::error`).

use thiserror::Error;

use toolkit_gts::gts_id;

/// Namespace of every OAGW error type, as an instance-id suffix.
const ERROR_TYPE_SEGMENT: &str = "cf.core.errors.err.v1~cf.oagw";

/// Problem extension member carrying the identifier of a referenced definition.
///
/// Declared here because the [`ManagementError::PluginInUse`] row carries it, so
/// the transport layer can only render it — never invent it.
pub const EXTENSION_PLUGIN_ID: &str = "plugin_id";

/// Problem extension member listing every referencing resource of a definition.
pub const EXTENSION_REFERENCED_BY: &str = "referenced_by";

/// Whether a caller may retry the failed request.
///
/// Mirrors the `Retriable` column of the error table; `Depends` records that
/// only the producing path can decide (the upstream said so).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retriable {
    /// Do not retry.
    No,
    /// Safe to retry.
    Yes,
    /// The upstream response decides; the proxy path classifies it.
    Depends,
}

// @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-gf-map-01
/// Domain errors raised by OAGW gear code.
///
/// Every variant maps to exactly one row of the error table: [`Self::status`],
/// [`Self::gts_id`] and [`Self::retriability`] are the table's HTTP status, GTS
/// Instance ID and `Retriable` column. Later entries (2.2 to 2.6) raise these
/// variants instead of re-implementing the table.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    /// General route validation error (400, not retriable).
    #[error("route error: {detail}")]
    RouteError {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Request validation failed (400, not retriable).
    #[error("validation failed: {detail}")]
    ValidationError {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// `X-OAGW-Target-Host` is required for a multi-endpoint upstream with a
    /// common suffix alias (400, not retriable).
    #[error("missing target host: {detail}")]
    MissingTargetHost {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// `X-OAGW-Target-Host` format is invalid (400, not retriable).
    #[error("invalid target host: {detail}")]
    InvalidTargetHost {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// `X-OAGW-Target-Host` does not match any configured endpoint (400, not
    /// retriable).
    #[error("unknown target host: {detail}")]
    UnknownTargetHost {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Cross-origin request from an origin the CORS configuration does not
    /// allow (403, not retriable).
    ///
    /// The two CORS rows come from ADR 0004 rather than from the DESIGN's
    /// interface table, which is why they are absent from its twenty rows.
    #[error("cors origin not allowed: {detail}")]
    CorsOriginNotAllowed {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Cross-origin request with a method the CORS configuration does not
    /// allow (403, not retriable).
    #[error("cors method not allowed: {detail}")]
    CorsMethodNotAllowed {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Authentication to the upstream failed (401, not retriable).
    #[error("authentication failed: {detail}")]
    AuthenticationFailed {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// No matching route found (404, not retriable).
    #[error("route not found: {detail}")]
    RouteNotFound {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Plugin in use (409, not retriable).
    #[error("plugin in use: {detail}")]
    PluginInUse {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Request payload exceeds the limit (413, not retriable).
    #[error("payload too large: {detail}")]
    PayloadTooLarge {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Rate limit exceeded (429, retriable).
    #[error("rate limit exceeded: {detail}")]
    RateLimitExceeded {
        /// Occurrence-specific explanation.
        detail: String,
        /// Retry guidance, in seconds.
        retry_after_seconds: Option<u32>,
    },

    /// Referenced secret not found (500, not retriable).
    #[error("secret not found: {detail}")]
    SecretNotFound {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Protocol-level error (502, not retriable).
    #[error("protocol error: {detail}")]
    ProtocolError {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Upstream service error (502, retriable: depends on the upstream).
    #[error("downstream error: {detail}")]
    DownstreamError {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Stream connection aborted (502, not retriable).
    #[error("stream aborted: {detail}")]
    StreamAborted {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Upstream link unavailable (503, retriable).
    #[error("link unavailable: {detail}")]
    LinkUnavailable {
        /// Occurrence-specific explanation.
        detail: String,
        /// Retry guidance, in seconds.
        retry_after_seconds: Option<u32>,
    },

    /// Circuit breaker open (503, retriable).
    #[error("circuit breaker open: {detail}")]
    CircuitBreakerOpen {
        /// Occurrence-specific explanation.
        detail: String,
        /// Retry guidance, in seconds.
        retry_after_seconds: Option<u32>,
    },

    /// Plugin not found (503, not retriable).
    #[error("plugin not found: {detail}")]
    PluginNotFound {
        /// Occurrence-specific explanation.
        detail: String,
    },

    /// Connection timeout (504, retriable).
    #[error("connection timeout: {detail}")]
    ConnectionTimeout {
        /// Occurrence-specific explanation.
        detail: String,
        /// Retry guidance, in seconds.
        retry_after_seconds: Option<u32>,
    },

    /// Request timeout (504, retriable).
    #[error("request timeout: {detail}")]
    RequestTimeout {
        /// Occurrence-specific explanation.
        detail: String,
        /// Retry guidance, in seconds.
        retry_after_seconds: Option<u32>,
    },

    /// Idle timeout (504, retriable).
    #[error("idle timeout: {detail}")]
    IdleTimeout {
        /// Occurrence-specific explanation.
        detail: String,
        /// Retry guidance, in seconds.
        retry_after_seconds: Option<u32>,
    },
}
// @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-gf-map-01

// @cpt-begin:cpt-cf-oagw-dod-sharing-and-permissions:p1:inst-full
/// Rejection of a management write that the error table has no row for.
///
/// The table of `cpt-cf-oagw-interface-api` fixes one row per data-plane
/// failure, so the management-only outcomes live here: the permission gate
/// (`403`) and the two conflict-class invariants (`409`). The GTS `type`
/// identifiers are the ones the entry-2.1 mapping layer defines in the
/// `gts.cf.core.errors.err.v1~cf.oagw` namespace, as section 1.2 records.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManagementRejection {
    /// Permission gate refused the write (403, not retriable).
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// `UNIQUE (tenant_id, alias)` violated (409, not retriable).
    #[error("alias conflict: {0}")]
    AliasConflict(String),

    /// Route match determinism violated (409, not retriable).
    #[error("route match conflict: {0}")]
    RouteMatchConflict(String),

    /// An immutable identity member was supplied with a different value
    /// (400, not retriable).
    #[error("immutable field: {0}")]
    Identity(String),
}

impl ManagementRejection {
    /// HTTP status code of the rejection.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::Forbidden(_) => 403,
            Self::AliasConflict(_) | Self::RouteMatchConflict(_) => 409,
            Self::Identity(_) => 400,
        }
    }

    /// GTS Instance ID of the rejection type.
    #[must_use]
    pub fn gts_id(&self) -> &'static str {
        match self {
            Self::Forbidden(_) => gts_id!(
                "cf.core.errors.err.v1~cf.oagw.management.forbidden.v1"
            ),
            Self::AliasConflict(_) => gts_id!(
                "cf.core.errors.err.v1~cf.oagw.management.alias_conflict.v1"
            ),
            Self::RouteMatchConflict(_) => gts_id!(
                "cf.core.errors.err.v1~cf.oagw.management.route_match_conflict.v1"
            ),
            Self::Identity(_) => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1")
            }
        }
    }

    /// Human-readable summary used as the problem `title`.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Self::Forbidden(_) => "Forbidden",
            Self::AliasConflict(_) => "Alias conflict",
            Self::RouteMatchConflict(_) => "Route match conflict",
            Self::Identity(_) => "Validation failed",
        }
    }

    /// Occurrence-specific explanation used as the problem `detail`.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Forbidden(detail)
            | Self::AliasConflict(detail)
            | Self::RouteMatchConflict(detail)
            | Self::Identity(detail) => detail,
        }
    }

    /// `Retriable` column: a management write is never worth retrying as-is.
    #[must_use]
    pub const fn retriability(&self) -> Retriable {
        Retriable::No
    }
}

/// Either a domain error of the fixed table or a management-only rejection.
///
/// The management handlers return this, so the mapping layer renders both
/// through the same `application/problem+json` projection.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManagementError {
    /// A row of the fixed error table.
    #[error(transparent)]
    Domain(#[from] DomainError),

    /// A management-only rejection.
    #[error(transparent)]
    Rejection(#[from] ManagementRejection),

    /// A plugin definition is still referenced and cannot be deleted
    /// (409, not retriable).
    ///
    /// The row of the fixed table carries the GTS `type` identifier, the title
    /// and the status; the conflict carries the two extension members the
    /// delete flow names (`cpt-cf-oagw-algo-plugin-in-use-scan`,
    /// `inst-pinu-10`): `plugin_id`, the definition's GTS identifier, and
    /// `referenced_by`, every referencing resource with its binding position or
    /// its `auth` position. The list names where the definition is used and
    /// nothing else — no configuration content of either side.
    #[error("plugin in use: {detail}")]
    PluginInUse {
        /// GTS identifier of the referenced definition.
        plugin_id: String,
        /// Every referencing resource identifier with its position.
        referenced_by: Vec<String>,
        /// Occurrence-specific explanation.
        detail: String,
    },
}

impl ManagementError {
    /// A `404` for a resource that does not resolve inside the calling tenant.
    ///
    /// An ancestor-owned resource is indistinguishable from a missing one, which
    /// is why the detail never names another tenant.
    #[must_use]
    pub fn not_found(detail: String) -> Self {
        Self::Domain(DomainError::RouteNotFound { detail })
    }

    /// A `400` for a body value the validators rejected.
    #[must_use]
    pub fn validation(detail: String) -> Self {
        Self::Domain(DomainError::ValidationError { detail })
    }

    /// A `403` for a permission the caller does not hold.
    #[must_use]
    pub fn forbidden(detail: String) -> Self {
        Self::Rejection(ManagementRejection::Forbidden(detail))
    }

    /// A `409` for an alias the tenant already uses.
    #[must_use]
    pub fn alias_conflict(detail: String) -> Self {
        Self::Rejection(ManagementRejection::AliasConflict(detail))
    }

    /// A `409` for a match rule another enabled route already serves.
    #[must_use]
    pub fn route_match_conflict(detail: String) -> Self {
        Self::Rejection(ManagementRejection::RouteMatchConflict(detail))
    }

    /// A `400` for an immutable identity member supplied with a new value.
    #[must_use]
    pub fn identity(detail: String) -> Self {
        Self::Rejection(ManagementRejection::Identity(detail))
    }

    /// A `409` for a plugin definition a binding or an `auth` reference still
    /// uses.
    #[must_use]
    pub fn plugin_in_use(plugin_id: String, referenced_by: Vec<String>) -> Self {
        let detail = if referenced_by.is_empty() {
            format!("plugin `{plugin_id}` is still referenced")
        } else {
            format!(
                "plugin `{plugin_id}` is still referenced by {}",
                referenced_by.join(", ")
            )
        };
        Self::PluginInUse {
            plugin_id,
            referenced_by,
            detail,
        }
    }

    /// HTTP status code of the mapped problem document.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::Domain(error) => error.status(),
            Self::Rejection(rejection) => rejection.status(),
            Self::PluginInUse { .. } => 409,
        }
    }

    /// GTS Instance ID of the mapped error type.
    #[must_use]
    pub fn gts_id(&self) -> &'static str {
        match self {
            Self::Domain(error) => error.gts_id(),
            Self::Rejection(rejection) => rejection.gts_id(),
            Self::PluginInUse { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1")
            }
        }
    }

    /// Human-readable summary used as the problem `title`.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Self::Domain(error) => error.title(),
            Self::Rejection(rejection) => rejection.title(),
            Self::PluginInUse { .. } => "Plugin in use",
        }
    }

    /// Occurrence-specific explanation used as the problem `detail`.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Domain(error) => error.detail(),
            Self::Rejection(rejection) => rejection.detail(),
            Self::PluginInUse { detail, .. } => detail,
        }
    }

    /// Retry guidance in seconds, when the producing path recorded it.
    ///
    /// Only a row of the fixed table can carry guidance; a management rejection
    /// is never worth retrying as-is, so it always maps to `None`.
    #[must_use]
    pub const fn retry_after_seconds(&self) -> Option<u32> {
        match self {
            Self::Domain(error) => error.retry_after_seconds(),
            Self::Rejection(_) => None,
            Self::PluginInUse { .. } => None,
        }
    }

    /// The extension members the error carries of its own, beyond the
    /// request-derived context members.
    ///
    /// Only the `PluginInUse` row carries any: `plugin_id` and `referenced_by`
    /// (`cpt-cf-oagw-dod-plugin-in-use-conflict`, `inst-full`). The mapping
    /// layer reads them through this accessor, so no other error row can grow
    /// extension members by accident.
    #[must_use]
    pub fn extension_members(&self) -> Vec<(String, serde_json::Value)> {
        match self {
            Self::PluginInUse {
                plugin_id,
                referenced_by,
                ..
            } => vec![
                (EXTENSION_PLUGIN_ID.to_owned(), serde_json::json!(plugin_id)),
                (
                    EXTENSION_REFERENCED_BY.to_owned(),
                    serde_json::json!(referenced_by),
                ),
            ],
            _ => Vec::new(),
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-sharing-and-permissions:p1:inst-full


// @cpt-begin:cpt-cf-oagw-dod-domain-error-mapping:p1:inst-full
impl DomainError {
    /// HTTP status code fixed by the error table.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::RouteError { .. }
            | Self::ValidationError { .. }
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => 400,
            Self::AuthenticationFailed { .. } => 401,
            Self::RouteNotFound { .. } => 404,
            Self::CorsOriginNotAllowed { .. } | Self::CorsMethodNotAllowed { .. } => 403,
            Self::PluginInUse { .. } => 409,
            Self::PayloadTooLarge { .. } => 413,
            Self::RateLimitExceeded { .. } => 429,
            Self::SecretNotFound { .. } => 500,

            Self::ProtocolError { .. }
            | Self::DownstreamError { .. }
            | Self::StreamAborted { .. } => 502,
            Self::LinkUnavailable { .. }
            | Self::CircuitBreakerOpen { .. }
            | Self::PluginNotFound { .. } => 503,
            Self::ConnectionTimeout { .. }
            | Self::RequestTimeout { .. }
            | Self::IdleTimeout { .. } => 504,
        }
    }

    /// GTS Instance ID of the error type, exactly as the table fixes it.
    #[must_use]
    pub fn gts_id(&self) -> &'static str {
        match self {
            Self::RouteError { .. } | Self::ValidationError { .. } => gts_id!(
                "cf.core.errors.err.v1~cf.oagw.validation.error.v1"
            ),
            Self::MissingTargetHost { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1")
            }
            Self::InvalidTargetHost { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1")
            }
            Self::UnknownTargetHost { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1")
            }
            Self::CorsOriginNotAllowed { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1")
            }
            Self::CorsMethodNotAllowed { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1")
            }
            Self::AuthenticationFailed { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1")
            }
            Self::RouteNotFound { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
            }
            Self::PluginInUse { .. } => gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"),
            Self::PayloadTooLarge { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1")
            }
            Self::RateLimitExceeded { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1")
            }
            Self::SecretNotFound { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1")
            }
            Self::ProtocolError { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1")
            }
            Self::DownstreamError { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1")
            }
            Self::StreamAborted { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1")
            }
            Self::LinkUnavailable { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1")
            }
            Self::CircuitBreakerOpen { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1")
            }
            Self::PluginNotFound { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1")
            }
            Self::ConnectionTimeout { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1")
            }
            Self::RequestTimeout { .. } => {
                gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1")
            }
            Self::IdleTimeout { .. } => gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"),
        }
    }

    /// Human-readable summary used as the problem `title`.
    #[must_use]
    pub const fn title(&self) -> &'static str {
        match self {
            Self::RouteError { .. } => "Route error",
            Self::ValidationError { .. } => "Validation failed",
            Self::MissingTargetHost { .. } => "Missing target host",
            Self::InvalidTargetHost { .. } => "Invalid target host",
            Self::UnknownTargetHost { .. } => "Unknown target host",
            Self::CorsOriginNotAllowed { .. } => "CORS origin not allowed",
            Self::CorsMethodNotAllowed { .. } => "CORS method not allowed",
            Self::AuthenticationFailed { .. } => "Authentication failed",
            Self::RouteNotFound { .. } => "Route not found",
            Self::PluginInUse { .. } => "Plugin in use",
            Self::PayloadTooLarge { .. } => "Payload too large",
            Self::RateLimitExceeded { .. } => "Rate limit exceeded",
            Self::SecretNotFound { .. } => "Secret not found",
            Self::ProtocolError { .. } => "Protocol error",
            Self::DownstreamError { .. } => "Downstream error",
            Self::StreamAborted { .. } => "Stream aborted",
            Self::LinkUnavailable { .. } => "Link unavailable",
            Self::CircuitBreakerOpen { .. } => "Circuit breaker open",
            Self::PluginNotFound { .. } => "Plugin not found",
            Self::ConnectionTimeout { .. } => "Connection timeout",
            Self::RequestTimeout { .. } => "Request timeout",
            Self::IdleTimeout { .. } => "Idle timeout",
        }
    }

    /// Occurrence-specific explanation used as the problem `detail`.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::RouteError { detail }
            | Self::ValidationError { detail }
            | Self::MissingTargetHost { detail }
            | Self::InvalidTargetHost { detail }
            | Self::UnknownTargetHost { detail }
            | Self::CorsOriginNotAllowed { detail }
            | Self::CorsMethodNotAllowed { detail }
            | Self::AuthenticationFailed { detail }
            | Self::RouteNotFound { detail }
            | Self::PluginInUse { detail }
            | Self::PayloadTooLarge { detail }
            | Self::RateLimitExceeded { detail, .. }
            | Self::SecretNotFound { detail }
            | Self::ProtocolError { detail }
            | Self::DownstreamError { detail }
            | Self::StreamAborted { detail }
            | Self::LinkUnavailable { detail, .. }
            | Self::CircuitBreakerOpen { detail, .. }
            | Self::PluginNotFound { detail }
            | Self::ConnectionTimeout { detail, .. }
            | Self::RequestTimeout { detail, .. }
            | Self::IdleTimeout { detail, .. } => detail,
        }
    }

    /// `Retriable` column of the error table.
    #[must_use]
    pub const fn retriability(&self) -> Retriable {
        match self {
            Self::RateLimitExceeded { .. }
            | Self::LinkUnavailable { .. }
            | Self::CircuitBreakerOpen { .. }
            | Self::ConnectionTimeout { .. }
            | Self::RequestTimeout { .. }
            | Self::IdleTimeout { .. } => Retriable::Yes,
            Self::DownstreamError { .. } => Retriable::Depends,
            Self::RouteError { .. }
            | Self::ValidationError { .. }
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. }
            | Self::CorsOriginNotAllowed { .. }
            | Self::CorsMethodNotAllowed { .. }
            | Self::AuthenticationFailed { .. }
            | Self::RouteNotFound { .. }
            | Self::PluginInUse { .. }
            | Self::PayloadTooLarge { .. }
            | Self::SecretNotFound { .. }
            | Self::ProtocolError { .. }
            | Self::StreamAborted { .. }
            | Self::PluginNotFound { .. } => Retriable::No,
        }
    }

    /// Retry guidance in seconds, when the producing path recorded it.
    #[must_use]
    pub const fn retry_after_seconds(&self) -> Option<u32> {
        match self {
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
            | Self::ConnectionTimeout {
                retry_after_seconds,
                ..
            }
            | Self::RequestTimeout {
                retry_after_seconds,
                ..
            }
            | Self::IdleTimeout {
                retry_after_seconds,
                ..
            } => *retry_after_seconds,
            _ => None,
        }
    }

    /// GTS type segment shared by every OAGW error type.
    #[must_use]
    pub const fn error_type_segment() -> &'static str {
        ERROR_TYPE_SEGMENT
    }
}
// @cpt-end:cpt-cf-oagw-dod-domain-error-mapping:p1:inst-full

impl toolkit::DomainErrorMarker for DomainError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row of the DESIGN error table plus the two ADR 0004 CORS rows,
    /// asserted variant by variant.
    fn table() -> [(&'static str, DomainError, u16, &'static str, Retriable); 22] {
        [
        (
            "RouteError",
            DomainError::RouteError {
                detail: "d".to_owned(),
            },
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Retriable::No,
        ),
        (
            "ValidationError",
            DomainError::ValidationError {
                detail: "d".to_owned(),
            },
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
            Retriable::No,
        ),
        (
            "MissingTargetHost",
            DomainError::MissingTargetHost {
                detail: "d".to_owned(),
            },
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1",
            Retriable::No,
        ),
        (
            "InvalidTargetHost",
            DomainError::InvalidTargetHost {
                detail: "d".to_owned(),
            },
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1",
            Retriable::No,
        ),
        (
            "UnknownTargetHost",
            DomainError::UnknownTargetHost {
                detail: "d".to_owned(),
            },
            400,
            "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1",
            Retriable::No,
        ),
        (
            "CorsOriginNotAllowed",
            DomainError::CorsOriginNotAllowed {
                detail: "d".to_owned(),
            },
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1",
            Retriable::No,
        ),
        (
            "CorsMethodNotAllowed",
            DomainError::CorsMethodNotAllowed {
                detail: "d".to_owned(),
            },
            403,
            "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1",
            Retriable::No,
        ),
        (
            "AuthenticationFailed",
            DomainError::AuthenticationFailed {
                detail: "d".to_owned(),
            },
            401,
            "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
            Retriable::No,
        ),
        (
            "RouteNotFound",
            DomainError::RouteNotFound {
                detail: "d".to_owned(),
            },
            404,
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1",
            Retriable::No,
        ),
        (
            "PluginInUse",
            DomainError::PluginInUse {
                detail: "d".to_owned(),
            },
            409,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
            Retriable::No,
        ),
        (
            "PayloadTooLarge",
            DomainError::PayloadTooLarge {
                detail: "d".to_owned(),
            },
            413,
            "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1",
            Retriable::No,
        ),
        (
            "RateLimitExceeded",
            DomainError::RateLimitExceeded {
                detail: "d".to_owned(),
                retry_after_seconds: Some(30),
            },
            429,
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1",
            Retriable::Yes,
        ),
        (
            "SecretNotFound",
            DomainError::SecretNotFound {
                detail: "d".to_owned(),
            },
            500,
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1",
            Retriable::No,
        ),
        (
            "ProtocolError",
            DomainError::ProtocolError {
                detail: "d".to_owned(),
            },
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1",
            Retriable::No,
        ),
        (
            "DownstreamError",
            DomainError::DownstreamError {
                detail: "d".to_owned(),
            },
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1",
            Retriable::Depends,
        ),
        (
            "StreamAborted",
            DomainError::StreamAborted {
                detail: "d".to_owned(),
            },
            502,
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1",
            Retriable::No,
        ),
        (
            "LinkUnavailable",
            DomainError::LinkUnavailable {
                detail: "d".to_owned(),
                retry_after_seconds: Some(5),
            },
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1",
            Retriable::Yes,
        ),
        (
            "CircuitBreakerOpen",
            DomainError::CircuitBreakerOpen {
                detail: "d".to_owned(),
                retry_after_seconds: Some(5),
            },
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1",
            Retriable::Yes,
        ),
        (
            "PluginNotFound",
            DomainError::PluginNotFound {
                detail: "d".to_owned(),
            },
            503,
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1",
            Retriable::No,
        ),
        (
            "ConnectionTimeout",
            DomainError::ConnectionTimeout {
                detail: "d".to_owned(),
                retry_after_seconds: Some(2),
            },
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1",
            Retriable::Yes,
        ),
        (
            "RequestTimeout",
            DomainError::RequestTimeout {
                detail: "d".to_owned(),
                retry_after_seconds: Some(2),
            },
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1",
            Retriable::Yes,
        ),
        (
            "IdleTimeout",
            DomainError::IdleTimeout {
                detail: "d".to_owned(),
                retry_after_seconds: Some(2),
            },
            504,
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1",
            Retriable::Yes,
        ),
        ]
    }

    #[test]
    fn every_table_row_resolves_its_status_type_and_retriability() {
        for (name, error, status, gts_id, retriable) in table() {
            assert_eq!(error.status(), status, "{name} status");
            assert_eq!(error.gts_id(), gts_id, "{name} GTS type");
            assert_eq!(error.retriability(), retriable, "{name} retriable");
            assert!(!error.title().is_empty(), "{name} title");
            assert_eq!(error.detail(), "d", "{name} detail");
        }
    }

    #[test]
    fn table_covers_every_row_with_unique_gts_types() {
        // The twenty rows of the DESIGN interface table plus the two ADR 0004
        // CORS rows.
        assert_eq!(table().len(), 22);
        let mut types: Vec<&str> = table().iter().map(|(_, e, ..)| e.gts_id()).collect();
        types.sort_unstable();
        types.dedup();
        // Only `RouteError` and `ValidationError` share a GTS type.
        assert_eq!(types.len(), 21);
    }

    #[test]
    fn gts_ids_are_prefixed_gts_identifiers() {
        for (_, error, ..) in table() {
            assert!(error.gts_id().starts_with("gts."), "{}", error.gts_id());
            assert!(error.gts_id().contains("~cf.oagw."), "{}", error.gts_id());
        }
    }

    #[test]
    fn only_retriable_rows_carry_retry_guidance() {
        for (_, error, ..) in &table() {
            let has_guidance = error.retry_after_seconds().is_some();
            assert_eq!(
                has_guidance,
                matches!(error.retriability(), Retriable::Yes),
                "{}",
                error.title()
            );
        }
    }

    #[test]
    fn retry_guidance_defaults_to_none() {
        let error = DomainError::RateLimitExceeded {
            detail: "too many requests".to_owned(),
            retry_after_seconds: None,
        };
        assert_eq!(error.retry_after_seconds(), None);
        assert_eq!(error.retriability(), Retriable::Yes);
    }

    #[test]
    fn domain_error_is_a_std_error() {
        let error: Box<dyn std::error::Error> = Box::new(DomainError::RouteNotFound {
            detail: "no route for /oagw/v1/x".to_owned(),
        });
        assert!(error.to_string().contains("route not found"));
    }

    #[test]
    fn domain_error_is_a_toolkit_domain_error_marker() {
        fn accepts_marker<T: toolkit::DomainErrorMarker>(_: &T) {}
        let error = DomainError::PluginInUse {
            detail: "guard plugin attached to a route".to_owned(),
        };
        accepts_marker(&error);
    }

    #[test]
    fn error_type_segment_is_the_shared_namespace() {
        assert_eq!(
            DomainError::error_type_segment(),
            "cf.core.errors.err.v1~cf.oagw"
        );
    }
}
