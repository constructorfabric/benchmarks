//! GTS type identifiers for OAGW resources.
//!
//! These mirror the identifiers documented in
//! `docs/schemas/*.json`, `DESIGN.md §3.3` and the error table
//! (`DESIGN.md §3.3 Error Response Format`).

use uuid::Uuid;

/// Upstream resource base type.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Route resource base type.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Auth plugin base type.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Guard plugin base type.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Transform plugin base type.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// Protocol base type.
pub const PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~";
/// Proxy resource base type (used for `:invoke` authorization).
pub const PROXY_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

/// Protocol child types.
pub const PROTOCOL_HTTP_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
pub const PROTOCOL_GRPC_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// --- Built-in auth plugins -------------------------------------------------

pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const AUTH_OAUTH2_CLIENT_CRED: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Catalog-only (no backing implementation).
pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only (no backing implementation).
pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

// --- Built-in guard plugins ------------------------------------------------

pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only (core Data Plane config via `proxy_timeout`).
pub const GUARD_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only (core Data Plane config via `upstream.cors`).
pub const GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

// --- Built-in transform plugins --------------------------------------------

pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only (core Data Plane instrumentation).
pub const TRANSFORM_LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only (core Data Plane instrumentation).
pub const TRANSFORM_METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// --- Error GTS instance ids (DESIGN §3.3 error table) ----------------------

/// Base segment shared by all OAGW gateway errors.
pub const ERROR_NS: &str = "gts.cf.core.errors.err.v1~cf.oagw.";
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
pub const ERR_PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
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
pub const ERR_TIMEOUT_CONNECTION: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
pub const ERR_TIMEOUT_REQUEST: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
pub const ERR_TIMEOUT_IDLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// Anonymous GTS instance id for an upstream.
#[must_use]
pub fn upstream_instance_id(id: Uuid) -> String {
    format!("{UPSTREAM_TYPE}{id}")
}

/// Anonymous GTS instance id for a route.
#[must_use]
pub fn route_instance_id(id: Uuid) -> String {
    format!("{ROUTE_TYPE}{id}")
}

/// Anonymous GTS instance id for a custom plugin.
#[must_use]
pub fn plugin_instance_id(kind: super::model::PluginKind, id: Uuid) -> String {
    format!("{}{id}", kind.base_type())
}

/// Extract the trailing UUID from an anonymous GTS instance id.
///
/// Returns `None` when the string is not a well-formed instance id of the
/// given base type.
#[must_use]
pub fn parse_instance_uuid(base_type: &str, value: &str) -> Option<Uuid> {
    let rest = value.strip_prefix(base_type)?;
    Uuid::parse_str(rest).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_roundtrip() {
        let id = Uuid::new_v4();
        let gid = upstream_instance_id(id);
        assert_eq!(parse_instance_uuid(UPSTREAM_TYPE, &gid), Some(id));
        assert_eq!(parse_instance_uuid(ROUTE_TYPE, &gid), None);
    }

    #[test]
    fn error_ids_match_design_table() {
        assert_eq!(ERR_VALIDATION, "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1");
        assert_eq!(ERR_RATE_LIMIT_EXCEEDED, "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
        assert_eq!(ERR_TIMEOUT_REQUEST, "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
        assert_eq!(ERR_CORS_ORIGIN_NOT_ALLOWED, "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
    }
}
