//! GTS identifier vocabulary for the OAGW gear.
//!
//! Centralises every OAGW GTS identifier used on the wire: error types for
//! data-plane problem responses, entity resource types for control-plane
//! canonical errors, protocol identifiers, and the built-in plugin catalog.
//!
//! All constants are compile-time-checked against the GTS identifier grammar
//! via the `gts_id!` macro.
//!
//! ## Error type shape
//!
//! Data-plane errors use the shared platform error resource type with an
//! OAGW-scoped instance segment:
//!
//! ```text
//! gts.cf.core.errors.err.v1~cf.oagw.<domain>.<name>.v1
//! ```

use toolkit_gts::gts_id;

// ---------------------------------------------------------------------------
// Entity resource types (control-plane canonical errors)
// ---------------------------------------------------------------------------

/// Canonical resource type for upstream entities.
pub const UPSTREAM_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Canonical resource type for route entities.
pub const ROUTE_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Canonical resource type for plugin entities.
pub const PLUGIN_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.plugin.v1~");

// ---------------------------------------------------------------------------
// Protocol identifiers
// ---------------------------------------------------------------------------

/// GTS identifier for the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// GTS identifier for the gRPC upstream protocol.
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

// ---------------------------------------------------------------------------
// Error types (data-plane problem responses)
// ---------------------------------------------------------------------------

/// Request validation failed.
pub const ERR_VALIDATION_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
/// Proxy target host header required but missing.
pub const ERR_MISSING_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");
/// Proxy target host header present but malformed.
pub const ERR_INVALID_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");
/// Proxy target host header does not match any upstream endpoint.
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");
/// Authentication with the upstream failed.
pub const ERR_AUTH_FAILED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
/// CORS origin rejected on an actual cross-origin request.
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
/// CORS method rejected on an actual cross-origin request.
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");
/// No route matched, or the upstream behind the alias was not found.
pub const ERR_ROUTE_NOT_FOUND: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
/// A plugin could not be deleted because it is still referenced.
pub const ERR_PLUGIN_IN_USE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
/// Request body exceeded the configured limit.
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
/// A rate-limit was exceeded.
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
/// A credential reference could not be resolved.
pub const ERR_SECRET_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1");
/// Protocol-level error talking to the upstream.
pub const ERR_PROTOCOL_ERROR: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1");
/// The downstream (client) connection failed.
pub const ERR_DOWNSTREAM_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
/// The upstream stream was aborted mid-transfer.
pub const ERR_STREAM_ABORTED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");
/// The upstream link is unavailable (disabled, or connectivity refused).
pub const ERR_LINK_UNAVAILABLE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
/// The upstream circuit breaker is open.
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1");
/// A referenced plugin could not be resolved.
pub const ERR_PLUGIN_NOT_FOUND: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");
/// Connecting to the upstream timed out.
pub const ERR_TIMEOUT_CONNECTION: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1");
/// Waiting for the upstream response timed out.
pub const ERR_TIMEOUT_REQUEST: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
/// The upstream connection idled out.
pub const ERR_TIMEOUT_IDLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");

// ---------------------------------------------------------------------------
// Built-in plugin catalog (types-registry-facing identifiers)
// ---------------------------------------------------------------------------

// Auth plugins.
pub const AUTH_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
pub const AUTH_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
pub const AUTH_OAUTH2_CLIENT_CRED: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// Catalog-only (no backing implementation): binding fails.
pub const AUTH_BASIC: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Catalog-only (no backing implementation): binding fails.
pub const AUTH_BEARER: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

// Guard plugins.
pub const GUARD_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Catalog-only: bound via the dedicated `cors` config surface, not
/// `plugins.items`.
pub const GUARD_CORS: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");
/// Catalog-only: bound via gear-level proxy timeout config, not
/// `plugins.items`.
pub const GUARD_TIMEOUT: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");

// Transform plugins.
pub const TRANSFORM_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Catalog-only (no backing implementation).
pub const TRANSFORM_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Catalog-only (no backing implementation).
pub const TRANSFORM_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// All catal/'d built-in plugin identifiers (any category).
pub const BUILTIN_PLUGIN_IDS: &[&str] = &[
    AUTH_NOOP,
    AUTH_APIKEY,
    AUTH_OAUTH2_CLIENT_CRED,
    AUTH_OAUTH2_CLIENT_CRED_BASIC,
    AUTH_BASIC,
    AUTH_BEARER,
    GUARD_REQUIRED_HEADERS,
    GUARD_CORS,
    GUARD_TIMEOUT,
    TRANSFORM_REQUEST_ID,
    TRANSFORM_LOGGING,
    TRANSFORM_METRICS,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gts_ids_are_wellformed() {
        for id in [
            UPSTREAM_RESOURCE_TYPE,
            ROUTE_RESOURCE_TYPE,
            PLUGIN_RESOURCE_TYPE,
            PROTOCOL_HTTP,
            PROTOCOL_GRPC,
            ERR_VALIDATION_ERROR,
            ERR_MISSING_TARGET_HOST,
            ERR_INVALID_TARGET_HOST,
            ERR_UNKNOWN_TARGET_HOST,
            ERR_AUTH_FAILED,
            ERR_ROUTE_NOT_FOUND,
            ERR_PLUGIN_IN_USE,
            ERR_PAYLOAD_TOO_LARGE,
            ERR_RATE_LIMIT_EXCEEDED,
            ERR_SECRET_NOT_FOUND,
            ERR_PROTOCOL_ERROR,
            ERR_DOWNSTREAM_ERROR,
            ERR_STREAM_ABORTED,
            ERR_LINK_UNAVAILABLE,
            ERR_CIRCUIT_BREAKER_OPEN,
            ERR_PLUGIN_NOT_FOUND,
            ERR_TIMEOUT_CONNECTION,
            ERR_TIMEOUT_REQUEST,
            ERR_TIMEOUT_IDLE,
        ] {
            assert!(
                toolkit_gts::GtsId::try_new(id).is_ok(),
                "invalid GTS id: {id}"
            );
        }
    }

    #[test]
    fn error_types_use_platform_error_resource() {
        assert!(ERR_VALIDATION_ERROR.starts_with("gts.cf.core.errors.err.v1~cf.oagw."));
        assert!(ERR_RATE_LIMIT_EXCEEDED.starts_with("gts.cf.core.errors.err.v1~cf.oagw."));
    }
}
