//! OAGW domain errors and their mapping to canonical errors.
//!
//! Every [`OagwError`] variant corresponds to one row of the error table in
//! `docs/DESIGN.md` (`cpt-cf-oagw-interface-api`, "Error Response Format"):
//! the variant carries that row's GTS instance id ([`OagwError::gts_id`]) and
//! its HTTP status, which the `CanonicalError` category reproduces.
//!
//! Handlers return `Result<T, CanonicalError>`; the canonical error middleware
//! renders the wire `Problem` (`application/problem+json`) with
//! `Problem::from_error` — never `from_error_debug` — so internal diagnostics
//! stay off the wire.

use toolkit_canonical_errors::{CanonicalError, Http, TransportOverride, resource_error};
use toolkit_gts::gts_id;

/// GTS type-id prefix shared by every OAGW error identifier: DESIGN.md maps
/// each error to `gts.cf.core.errors.err.v1~<fragment>`.
pub const ERROR_GTS_DOMAIN: &str = gts_id!("cf.core.errors.err.v1~");

// --- 4xx: gateway-side request and routing failures -------------------------

/// 400 `ValidationError` / `RouteError` — request or domain validation failed.
pub const VALIDATION_ERROR_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
/// 400 `MissingTargetHost`.
pub const MISSING_TARGET_HOST_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");
/// 400 `InvalidTargetHost`.
pub const INVALID_TARGET_HOST_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");
/// 400 `UnknownTargetHost`.
pub const UNKNOWN_TARGET_HOST_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");
/// 401 `AuthenticationFailed`.
pub const AUTHENTICATION_FAILED_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
/// 404 `RouteNotFound`.
pub const ROUTE_NOT_FOUND_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
/// 409 alias conflict — DESIGN.md "CRUD Semantics" makes the alias unique per
/// `(tenant_id, alias)` and names the id in the phase rules
/// (`cf.oagw.alias.conflict.v1`).
pub const ALIAS_CONFLICT_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.alias.conflict.v1");
/// 409 `PluginInUse`.
pub const PLUGIN_IN_USE_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
/// 413 `PayloadTooLarge`.
pub const PAYLOAD_TOO_LARGE_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
/// 429 `RateLimitExceeded`.
pub const RATE_LIMIT_EXCEEDED_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");

// --- 5xx: upstream and data-plane failures ----------------------------------

/// 500 `SecretNotFound`.
pub const SECRET_NOT_FOUND_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1");
/// 500 `SecretError` — the credential store itself could not be consulted
/// (phase-4 rule: a resolver error is a gateway 500, distinct from a missing
/// reference).
pub const SECRET_ERROR_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.secret_error.v1");
/// 502 `ProtocolError`.
pub const PROTOCOL_ERROR_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1");
/// 502 `DownstreamError`.
pub const DOWNSTREAM_ERROR_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
/// 502 `StreamAborted`.
pub const STREAM_ABORTED_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");
/// 503 `LinkUnavailable`.
pub const LINK_UNAVAILABLE_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
/// 503 `CircuitBreakerOpen`.
pub const CIRCUIT_BREAKER_OPEN_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1");
/// 503 `PluginNotFound`.
pub const PLUGIN_NOT_FOUND_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");
/// 504 `ConnectionTimeout`.
pub const CONNECTION_TIMEOUT_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1");
/// 504 `RequestTimeout`.
pub const REQUEST_TIMEOUT_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
/// 504 `IdleTimeout`.
pub const IDLE_TIMEOUT_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");

/// Wire `reason` for the `Aborted` (409) mapping of [`OagwError::PluginInUse`]:
/// the delete is rejected because the plugin is still referenced. Matches the
/// `reason::aborted::CONFLICT` value other gears emit for conflicts.
const PLUGIN_IN_USE_REASON: &str = "CONFLICT";

/// 413 status override for the `PayloadTooLarge` mapping (same status class as
/// the `InvalidArgument` default of 400).
const PAYLOAD_TOO_LARGE_STATUS: TransportOverride = Http::status_code(413);
/// 502 status override for the upstream-failure mappings (`Internal` defaults
/// to 500 — same status class).
const BAD_GATEWAY_STATUS: TransportOverride = Http::status_code(502);

// One resource type per OAGW error GTS id whose canonical category is able to
// carry one: the wire `Problem.type` stays the canonical category id and the
// DESIGN.md table id travels in `context.resource_type` (in type form, i.e.
// with the trailing `~`). `unauthenticated`, `internal` and
// `service_unavailable` have no resource-type-carrying builder in
// `toolkit-canonical-errors`, so those variants map onto a bare canonical
// error and keep their GTS id on [`OagwError::gts_id`].

/// 400 — [`OagwError::Validation`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1~"))]
pub struct OagwValidationError;
/// 400 — [`OagwError::MissingTargetHost`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1~"))]
pub struct OagwMissingTargetHost;
/// 400 — [`OagwError::InvalidTargetHost`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1~"))]
pub struct OagwInvalidTargetHost;
/// 400 — [`OagwError::UnknownTargetHost`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1~"))]
pub struct OagwUnknownTargetHost;
/// 404 — [`OagwError::RouteNotFound`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1~"))]
pub struct OagwRouteNotFound;
/// 409 — [`OagwError::AliasConflict`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.alias.conflict.v1~"))]
pub struct OagwAliasConflict;
/// 409 — [`OagwError::PluginInUse`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1~"))]
pub struct OagwPluginInUse;
/// 413 — [`OagwError::PayloadTooLarge`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1~"))]
pub struct OagwPayloadTooLarge;
/// 429 — [`OagwError::RateLimitExceeded`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1~"))]
pub struct OagwRateLimitExceeded;
/// 504 — [`OagwError::ConnectionTimeout`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1~"))]
pub struct OagwConnectionTimeout;
/// 504 — [`OagwError::RequestTimeout`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1~"))]
pub struct OagwRequestTimeout;
/// 504 — [`OagwError::IdleTimeout`].
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1~"))]
pub struct OagwIdleTimeout;

/// Domain error for the OAGW management API and the proxy data plane.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OagwError {
    /// 400 `cf.oagw.validation.error.v1` — request or domain validation failed.
    #[error("validation failed: {message}")]
    Validation {
        /// Human-readable, client-safe description of the violation.
        message: String,
    },
    /// 400 `cf.oagw.routing.missing_target_host.v1` — `X-OAGW-Target-Host` is
    /// required for a multi-endpoint upstream with a common-suffix alias.
    #[error("X-OAGW-Target-Host header is required: {message}")]
    MissingTargetHost {
        /// Human-readable, client-safe detail.
        message: String,
    },
    /// 400 `cf.oagw.routing.invalid_target_host.v1` — the `X-OAGW-Target-Host`
    /// value is malformed (hostname or IP, no port/path).
    #[error("invalid X-OAGW-Target-Host value: {message}")]
    InvalidTargetHost {
        /// Human-readable, client-safe description of the violation.
        message: String,
    },
    /// 400 `cf.oagw.routing.unknown_target_host.v1` — the `X-OAGW-Target-Host`
    /// value matches no configured endpoint.
    #[error("unknown X-OAGW-Target-Host value: {message}")]
    UnknownTargetHost {
        /// Human-readable, client-safe description of the violation.
        message: String,
    },
    /// 401 `cf.oagw.auth.failed.v1` — authentication to the upstream failed.
    #[error("authentication to the upstream failed: {message}")]
    AuthenticationFailed {
        /// Human-readable, client-safe description of the failure.
        message: String,
    },
    /// 404 `cf.oagw.route.not_found.v1` — no route matched the request.
    #[error("no matching route found")]
    RouteNotFound {
        /// Alias the request was routed through.
        alias: String,
    },
    /// 409 `cf.oagw.alias.conflict.v1` — the alias is already in use.
    #[error("alias '{alias}' is already in use")]
    AliasConflict {
        /// Conflicting alias.
        alias: String,
    },
    /// 409 `cf.oagw.plugin.in_use.v1` — the plugin is still referenced.
    #[error("plugin '{plugin_id}' is still referenced")]
    PluginInUse {
        /// Id of the plugin that cannot be deleted.
        plugin_id: String,
    },
    /// 413 `cf.oagw.payload.too_large.v1` — request body exceeds the limit.
    #[error("request payload exceeds the configured limit: {message}")]
    PayloadTooLarge {
        /// Human-readable, client-safe description of the limit hit.
        message: String,
    },
    /// 429 `cf.oagw.rate_limit.exceeded.v1` — rate limit exceeded.
    #[error("rate limit exceeded")]
    RateLimitExceeded {
        /// Suggested retry hint, in seconds.
        retry_after_seconds: u64,
    },
    /// 500 `cf.oagw.secret.not_found.v1` — referenced secret is missing.
    #[error("referenced secret not found")]
    SecretNotFound,
    /// 500 `cf.oagw.downstream.secret_error.v1` — the credential store could
    /// not be consulted. The store's own diagnostics stay out of the message.
    #[error("the credential store could not be consulted")]
    SecretError {
        /// Operator-facing detail; never contains secret material.
        message: String,
    },
    /// 502 `cf.oagw.protocol.error.v1` — protocol-level error.
    #[error("protocol-level error from the upstream: {message}")]
    ProtocolError {
        /// Human-readable, client-safe description of the failure.
        message: String,
    },
    /// 502 `cf.oagw.downstream.error.v1` — upstream service error.
    #[error("upstream service error: {message}")]
    DownstreamError {
        /// Human-readable, client-safe description of the failure.
        message: String,
    },
    /// 502 `cf.oagw.stream.aborted.v1` — stream connection aborted.
    #[error("stream connection aborted: {message}")]
    StreamAborted {
        /// Human-readable, client-safe description of the abort.
        message: String,
    },
    /// 503 `cf.oagw.link.unavailable.v1` — upstream link unavailable.
    #[error("upstream link unavailable: {message}")]
    LinkUnavailable {
        /// Human-readable, client-safe description of the outage.
        message: String,
    },
    /// 503 `cf.oagw.circuit_breaker.open.v1` — circuit breaker open.
    #[error("circuit breaker open: {message}")]
    CircuitBreakerOpen {
        /// Human-readable, client-safe description of the state.
        message: String,
    },
    /// 503 `cf.oagw.plugin.not_found.v1` — bound plugin is not registered.
    #[error("plugin not found: {message}")]
    PluginNotFound {
        /// Human-readable, client-safe description of the missing plugin.
        message: String,
    },
    /// 504 `cf.oagw.timeout.connection.v1` — connection timed out.
    #[error("connection to the upstream timed out: {message}")]
    ConnectionTimeout {
        /// Human-readable, client-safe description of the timeout.
        message: String,
    },
    /// 504 `cf.oagw.timeout.request.v1` — request timed out.
    #[error("upstream request timed out: {message}")]
    RequestTimeout {
        /// Human-readable, client-safe description of the timeout.
        message: String,
    },
    /// 504 `cf.oagw.timeout.idle.v1` — idle stream timed out.
    #[error("idle stream timed out: {message}")]
    IdleTimeout {
        /// Human-readable, client-safe description of the timeout.
        message: String,
    },
}

impl OagwError {
    /// GTS instance id of this error, as listed in DESIGN.md's error table.
    #[must_use]
    pub fn gts_id(&self) -> &'static str {
        match self {
            Self::Validation { .. } => VALIDATION_ERROR_GTS_ID,
            Self::MissingTargetHost { .. } => MISSING_TARGET_HOST_GTS_ID,
            Self::InvalidTargetHost { .. } => INVALID_TARGET_HOST_GTS_ID,
            Self::UnknownTargetHost { .. } => UNKNOWN_TARGET_HOST_GTS_ID,
            Self::AuthenticationFailed { .. } => AUTHENTICATION_FAILED_GTS_ID,
            Self::RouteNotFound { .. } => ROUTE_NOT_FOUND_GTS_ID,
            Self::AliasConflict { .. } => ALIAS_CONFLICT_GTS_ID,
            Self::PluginInUse { .. } => PLUGIN_IN_USE_GTS_ID,
            Self::PayloadTooLarge { .. } => PAYLOAD_TOO_LARGE_GTS_ID,
            Self::RateLimitExceeded { .. } => RATE_LIMIT_EXCEEDED_GTS_ID,
            Self::SecretNotFound => SECRET_NOT_FOUND_GTS_ID,
            Self::SecretError { .. } => SECRET_ERROR_GTS_ID,
            Self::ProtocolError { .. } => PROTOCOL_ERROR_GTS_ID,
            Self::DownstreamError { .. } => DOWNSTREAM_ERROR_GTS_ID,
            Self::StreamAborted { .. } => STREAM_ABORTED_GTS_ID,
            Self::LinkUnavailable { .. } => LINK_UNAVAILABLE_GTS_ID,
            Self::CircuitBreakerOpen { .. } => CIRCUIT_BREAKER_OPEN_GTS_ID,
            Self::PluginNotFound { .. } => PLUGIN_NOT_FOUND_GTS_ID,
            Self::ConnectionTimeout { .. } => CONNECTION_TIMEOUT_GTS_ID,
            Self::RequestTimeout { .. } => REQUEST_TIMEOUT_GTS_ID,
            Self::IdleTimeout { .. } => IDLE_TIMEOUT_GTS_ID,
        }
    }

    /// HTTP status of this error, per DESIGN.md's error table.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::Validation { .. }
            | Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. } => 400,
            Self::AuthenticationFailed { .. } => 401,
            Self::RouteNotFound { .. } => 404,
            Self::AliasConflict { .. } | Self::PluginInUse { .. } => 409,
            Self::PayloadTooLarge { .. } => 413,
            Self::RateLimitExceeded { .. } => 429,
            Self::SecretNotFound | Self::SecretError { .. } => 500,
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
}

impl From<OagwError> for CanonicalError {
    fn from(error: OagwError) -> Self {
        // 400/404/409/413/429/504 rows map onto resource-type-carrying
        // categories; `SecretNotFound`, `SecretError`, the 502s and the 503s map
        // onto `internal`, `unauthenticated` and `service_unavailable`, which
        // accept no resource type — their GTS id stays on `OagwError::gts_id`
        // and their internal description is kept off the wire.
        match error {
            OagwError::Validation { message } => OagwValidationError::invalid_argument()
                .with_format(message)
                .create(),
            OagwError::MissingTargetHost { message } => OagwMissingTargetHost::invalid_argument()
                .with_format(message)
                .create(),
            OagwError::InvalidTargetHost { message } => OagwInvalidTargetHost::invalid_argument()
                .with_format(message)
                .create(),
            OagwError::UnknownTargetHost { message } => OagwUnknownTargetHost::invalid_argument()
                .with_format(message)
                .create(),
            OagwError::AuthenticationFailed { message } => CanonicalError::unauthenticated()
                .with_reason(message)
                .create(),
            OagwError::RouteNotFound { alias } => OagwRouteNotFound::not_found(format!(
                "no route matches the request for alias '{alias}'"
            ))
            .with_resource(alias)
            .create(),
            OagwError::AliasConflict { alias } => {
                OagwAliasConflict::already_exists(format!("alias '{alias}' is already in use"))
                    .with_resource(alias)
                    .create()
            }
            OagwError::PluginInUse { plugin_id } => {
                OagwPluginInUse::aborted(format!("plugin '{plugin_id}' is still referenced"))
                    .with_resource(plugin_id)
                    .with_reason(PLUGIN_IN_USE_REASON)
                    .create()
            }
            OagwError::PayloadTooLarge { message } => OagwPayloadTooLarge::invalid_argument()
                .with_format(message)
                .with_override(PAYLOAD_TOO_LARGE_STATUS)
                .create(),
            OagwError::RateLimitExceeded {
                retry_after_seconds,
            } => OagwRateLimitExceeded::resource_exhausted("OAGW rate limit exceeded")
                .with_quota_violation(
                    "rate_limit",
                    "Rate limit exceeded for this scope; retry later",
                )
                .with_quota_violation_retry_after_seconds(retry_after_seconds)
                .create(),
            OagwError::SecretNotFound => {
                CanonicalError::internal("referenced secret not found").create()
            }
            OagwError::SecretError { message } => CanonicalError::internal(message).create(),
            OagwError::ProtocolError { message }
            | OagwError::DownstreamError { message }
            | OagwError::StreamAborted { message } => CanonicalError::internal(message)
                .with_override(BAD_GATEWAY_STATUS)
                .create(),
            OagwError::LinkUnavailable { message }
            | OagwError::CircuitBreakerOpen { message }
            | OagwError::PluginNotFound { message } => CanonicalError::service_unavailable()
                .with_detail(message)
                .create(),
            OagwError::ConnectionTimeout { message } => {
                OagwConnectionTimeout::deadline_exceeded(message).create()
            }
            OagwError::RequestTimeout { message } => {
                OagwRequestTimeout::deadline_exceeded(message).create()
            }
            OagwError::IdleTimeout { message } => {
                OagwIdleTimeout::deadline_exceeded(message).create()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use toolkit_canonical_errors::{CanonicalError, Problem};

    use super::{ERROR_GTS_DOMAIN, OagwError};
    use crate::domain::error::{
        ALIAS_CONFLICT_GTS_ID, AUTHENTICATION_FAILED_GTS_ID, CIRCUIT_BREAKER_OPEN_GTS_ID,
        CONNECTION_TIMEOUT_GTS_ID, DOWNSTREAM_ERROR_GTS_ID, IDLE_TIMEOUT_GTS_ID,
        INVALID_TARGET_HOST_GTS_ID, LINK_UNAVAILABLE_GTS_ID, MISSING_TARGET_HOST_GTS_ID,
        PAYLOAD_TOO_LARGE_GTS_ID, PLUGIN_IN_USE_GTS_ID, PLUGIN_NOT_FOUND_GTS_ID,
        PROTOCOL_ERROR_GTS_ID, RATE_LIMIT_EXCEEDED_GTS_ID, REQUEST_TIMEOUT_GTS_ID,
        ROUTE_NOT_FOUND_GTS_ID, SECRET_ERROR_GTS_ID, SECRET_NOT_FOUND_GTS_ID,
        STREAM_ABORTED_GTS_ID, UNKNOWN_TARGET_HOST_GTS_ID, VALIDATION_ERROR_GTS_ID,
    };

    fn validation() -> OagwError {
        OagwError::Validation {
            message: "alias must not be empty".to_owned(),
        }
    }

    fn detail() -> String {
        "detail".to_owned()
    }

    /// One entry per row of DESIGN.md's error table, in table order.
    fn error_table() -> Vec<(OagwError, u16, &'static str)> {
        let alias = "api.vendor.com".to_owned();
        let plugin_id = "00000000-0000-0000-0000-000000000001".to_owned();
        vec![
            (validation(), 400, VALIDATION_ERROR_GTS_ID),
            (
                OagwError::MissingTargetHost { message: detail() },
                400,
                MISSING_TARGET_HOST_GTS_ID,
            ),
            (
                OagwError::InvalidTargetHost { message: detail() },
                400,
                INVALID_TARGET_HOST_GTS_ID,
            ),
            (
                OagwError::UnknownTargetHost { message: detail() },
                400,
                UNKNOWN_TARGET_HOST_GTS_ID,
            ),
            (
                OagwError::AuthenticationFailed { message: detail() },
                401,
                AUTHENTICATION_FAILED_GTS_ID,
            ),
            (
                OagwError::RouteNotFound {
                    alias: alias.clone(),
                },
                404,
                ROUTE_NOT_FOUND_GTS_ID,
            ),
            (
                OagwError::AliasConflict {
                    alias: alias.clone(),
                },
                409,
                ALIAS_CONFLICT_GTS_ID,
            ),
            (
                OagwError::PluginInUse {
                    plugin_id: plugin_id.clone(),
                },
                409,
                PLUGIN_IN_USE_GTS_ID,
            ),
            (
                OagwError::PayloadTooLarge { message: detail() },
                413,
                PAYLOAD_TOO_LARGE_GTS_ID,
            ),
            (
                OagwError::RateLimitExceeded {
                    retry_after_seconds: 5,
                },
                429,
                RATE_LIMIT_EXCEEDED_GTS_ID,
            ),
            (OagwError::SecretNotFound, 500, SECRET_NOT_FOUND_GTS_ID),
            (
                OagwError::SecretError { message: detail() },
                500,
                SECRET_ERROR_GTS_ID,
            ),
            (
                OagwError::ProtocolError { message: detail() },
                502,
                PROTOCOL_ERROR_GTS_ID,
            ),
            (
                OagwError::DownstreamError { message: detail() },
                502,
                DOWNSTREAM_ERROR_GTS_ID,
            ),
            (
                OagwError::StreamAborted { message: detail() },
                502,
                STREAM_ABORTED_GTS_ID,
            ),
            (
                OagwError::LinkUnavailable { message: detail() },
                503,
                LINK_UNAVAILABLE_GTS_ID,
            ),
            (
                OagwError::CircuitBreakerOpen { message: detail() },
                503,
                CIRCUIT_BREAKER_OPEN_GTS_ID,
            ),
            (
                OagwError::PluginNotFound { message: detail() },
                503,
                PLUGIN_NOT_FOUND_GTS_ID,
            ),
            (
                OagwError::ConnectionTimeout { message: detail() },
                504,
                CONNECTION_TIMEOUT_GTS_ID,
            ),
            (
                OagwError::RequestTimeout { message: detail() },
                504,
                REQUEST_TIMEOUT_GTS_ID,
            ),
            (
                OagwError::IdleTimeout { message: detail() },
                504,
                IDLE_TIMEOUT_GTS_ID,
            ),
        ]
    }

    #[test]
    fn every_variant_maps_to_its_design_error_table_row() {
        for (error, status, gts_id) in error_table() {
            assert_eq!(error.http_status(), status, "declared status for {error}");
            assert_eq!(error.gts_id(), gts_id, "gts id for {error}");

            let canonical = CanonicalError::from(error.clone());
            assert_eq!(
                canonical.status_code(),
                status,
                "canonical status for {error}"
            );
            assert_eq!(error.to_string(), format!("{error}"));
        }
    }

    #[test]
    fn every_gts_id_shares_the_canonical_error_domain() {
        assert_eq!(ERROR_GTS_DOMAIN, "gts.cf.core.errors.err.v1~");
        for (error, _, _) in error_table() {
            assert!(
                error.gts_id().starts_with(ERROR_GTS_DOMAIN),
                "{error}: {} is not in the canonical error domain",
                error.gts_id()
            );
        }
    }

    #[test]
    fn validation_gts_id_matches_the_design_table_literal() {
        assert_eq!(
            VALIDATION_ERROR_GTS_ID,
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[test]
    fn canonical_error_carries_the_oagw_gts_id_as_resource_type() {
        for (error, _, gts_id) in error_table() {
            let canonical = CanonicalError::from(error.clone());
            let Some(resource_type) = canonical.resource_type() else {
                // `unauthenticated`, `internal` and `service_unavailable` have
                // no resource-type-carrying builder in the toolkit.
                continue;
            };
            assert_eq!(
                resource_type.trim_end_matches('~'),
                gts_id,
                "resource type for {error}"
            );
        }
    }

    #[test]
    fn validation_problem_keeps_the_client_safe_message() {
        let problem = Problem::from(CanonicalError::from(validation()));
        assert_eq!(problem.status, 400);
        // A 400 validation message is written for the caller, so it may be
        // echoed on the wire — both as `detail` and as the `format` extension.
        assert_eq!(problem.detail, "alias must not be empty");
        assert_eq!(problem.context["format"], "alias must not be empty");
        assert!(
            problem
                .problem_type
                .contains("cf.core.err.invalid_argument.v1"),
            "problem type: {}",
            problem.problem_type
        );
    }

    #[test]
    fn upstream_error_problem_hides_the_internal_description() {
        let problem = Problem::from(CanonicalError::from(OagwError::ProtocolError {
            message: "httparse error at byte 12".to_owned(),
        }));
        assert_eq!(problem.status, 502);
        assert!(
            !problem.detail.contains("httparse"),
            "internal description leaked: {}",
            problem.detail
        );
        assert!(
            !problem.context.to_string().contains("httparse"),
            "internal description leaked: {}",
            problem.context
        );
    }

    #[test]
    fn secret_not_found_problem_stays_generic() {
        // `SecretNotFound` is a 500: the reference id stays off the wire.
        let problem = Problem::from(CanonicalError::from(OagwError::SecretNotFound));
        assert_eq!(problem.status, 500);
        assert!(
            !problem.detail.to_lowercase().contains("secret"),
            "internal description leaked: {}",
            problem.detail
        );
    }

    #[test]
    fn conflict_mappings_carry_the_resource() {
        let conflict = CanonicalError::from(OagwError::AliasConflict {
            alias: "api.vendor.com".to_owned(),
        });
        assert_eq!(conflict.status_code(), 409);
        assert_eq!(conflict.resource_name(), Some("api.vendor.com"));

        let in_use = CanonicalError::from(OagwError::PluginInUse {
            plugin_id: "00000000-0000-0000-0000-000000000001".to_owned(),
        });
        assert_eq!(in_use.status_code(), 409);
        assert_eq!(
            in_use.resource_name(),
            Some("00000000-0000-0000-0000-000000000001")
        );
    }

    #[test]
    fn rate_limit_mapping_reports_the_retry_hint() {
        let canonical = CanonicalError::from(OagwError::RateLimitExceeded {
            retry_after_seconds: 7,
        });
        assert_eq!(canonical.status_code(), 429);

        let problem = Problem::from(canonical);
        assert_eq!(
            problem.context["violations"][0]["retry_after_seconds"],
            serde_json::json!(7)
        );
    }

    #[test]
    fn service_unavailable_mapping_keeps_a_client_safe_detail() {
        let problem = Problem::from(CanonicalError::from(OagwError::LinkUnavailable {
            message: "upstream link unavailable".to_owned(),
        }));
        assert_eq!(problem.status, 503);
        assert_eq!(problem.detail, "upstream link unavailable");
    }
}
