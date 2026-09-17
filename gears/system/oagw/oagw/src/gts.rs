//! GTS identifier constants and helpers for the OAGW gear.
//!
//! All OAGW resources use anonymous GTS identifiers
//! (`gts.cf.core.oagw.{type}.v1~{instance}`) and gateway errors use the
//! platform error type (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`). Keeping
//! the identifiers in one module prevents drift between the control plane,
//! the data plane, and the error mapper.

use uuid::Uuid;

// -- resource types ---------------------------------------------------------

/// Upstream resource type.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1";
/// Route resource type.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1";
/// Auth plugin resource type.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1";
/// Guard plugin resource type.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1";
/// Transform plugin resource type.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1";

/// Protocol identifiers for upstreams.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC protocol identifier (catalog-only; no gRPC proxy path is implemented).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Error type prefix for every OAGW gateway error.
pub const ERR_BASE: &str = "gts.cf.core.errors.err.v1";

// -- gateway error identifiers (instance part) ------------------------------

pub const ERR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
pub const ERR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
pub const ERR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
pub const ERR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
pub const ERR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
pub const ERR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
pub const ERR_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
pub const ERR_PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
pub const ERR_DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
pub const ERR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
pub const ERR_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
pub const ERR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
pub const ERR_CONNECTION_TIMEOUT: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
pub const ERR_REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
pub const ERR_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";

// -- built-in plugin identifiers ---------------------------------------------

pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const AUTH_OAUTH2_CC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const AUTH_OAUTH2_CC_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Catalog-only (no backing implementation).
pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only (no backing implementation).
pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Split a full GTS identifier into its `~` instance part.
pub fn instance_part(full: &str) -> Option<&str> {
    full.rsplit_once('~').map(|(_, instance)| instance)
}

/// Whether the identifier's instance part is a UUID (custom plugin).
pub fn is_uuid_instance(full: &str) -> bool {
    instance_part(full)
        .map(|i| Uuid::parse_str(i).is_ok())
        .unwrap_or(false)
}

/// The UUID instance part of a UUID-backed plugin identifier (custom plugin
/// reference), if the instance is a valid UUID.
pub fn plugin_uuid(full: &str) -> Option<Uuid> {
    instance_part(full).and_then(|i| Uuid::parse_str(i).ok())
}

/// Resource type (base GTS identifier prefix, up to `~`) of a plugin type.
pub fn plugin_base(plugin_type: &str) -> &'static str {
    match plugin_type {
        p if p.starts_with(AUTH_PLUGIN_TYPE) => AUTH_PLUGIN_TYPE,
        p if p.starts_with(GUARD_PLUGIN_TYPE) => GUARD_PLUGIN_TYPE,
        p if p.starts_with(TRANSFORM_PLUGIN_TYPE) => TRANSFORM_PLUGIN_TYPE,
        _ => "",
    }
}
