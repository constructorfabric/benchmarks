//! GTS identifier constants for the OAGW gear.
//!
//! These strings are the single source of truth for every GTS identifier
//! the OAGW catalogs in the types-registry and resolves at runtime:
//! resource type ids, built-in plugin instance ids, protocol instances and
//! RFC 9457 problem `type` ids.

/// Prefix for upstream resource type ids.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// Prefix for route resource type ids.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// Prefix for auth plugin type ids.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Prefix for guard plugin type ids.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Prefix for transform plugin type ids.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// HTTP upstream protocol instance id.
pub const HTTP_PROTOCOL_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC upstream protocol instance id (catalog-only for now; no gRPC proxy
/// code path is implemented or reachable).
pub const GRPC_PROTOCOL_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in auth plugins
// ---------------------------------------------------------------------------

/// `noop` auth plugin — no authentication.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `apikey` auth plugin — API key injection (header/query).
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `oauth2_client_cred` auth plugin — `OAuth2` client credentials (Form).
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `oauth2_client_cred_basic` auth plugin — `OAuth2` client credentials
/// (Basic).
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// `basic` auth plugin — catalog identifier only, no backing implementation.
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// `bearer` auth plugin — catalog identifier only, no backing implementation.
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

// ---------------------------------------------------------------------------
// Built-in guard plugins
// ---------------------------------------------------------------------------

/// `required_headers` guard plugin — request/response header presence
/// enforcement. The only guard identifier resolvable via
/// `GuardPluginRegistry`.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// `timeout` guard plugin — catalog identifier only; implemented as core
/// Data Plane logic.
pub const TIMEOUT_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// `cors` guard plugin — catalog identifier only; implemented as core Data
/// Plane logic.
pub const CORS_GUARD_PLUGIN_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

// ---------------------------------------------------------------------------
// Built-in transform plugins
// ---------------------------------------------------------------------------

/// `request_id` transform plugin — `X-Request-ID` propagation.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// `logging` transform plugin — catalog identifier only; core Data Plane
/// instrumentation.
pub const LOGGING_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// `metrics` transform plugin — catalog identifier only; core Data Plane
/// instrumentation.
pub const METRICS_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// ---------------------------------------------------------------------------
// RFC 9457 error type ids (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`)
// ---------------------------------------------------------------------------

const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";

/// 400 — general validation error / route-level validation error.
pub const ERROR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
/// 400 — `X-OAGW-Target-Host` required for a multi-endpoint upstream with a
/// common-suffix alias.
pub const ERROR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
/// 400 — `X-OAGW-Target-Host` format is invalid.
pub const ERROR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
/// 400 — `X-OAGW-Target-Host` does not match any configured endpoint.
pub const ERROR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
/// 403 — CORS origin not allowed.
pub const ERROR_CORS_ORIGIN_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// 403 — CORS method not allowed.
pub const ERROR_CORS_METHOD_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";
/// 401 — authentication to the upstream failed.
pub const ERROR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
/// 404 — no matching route found.
pub const ERROR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// 409 — plugin is in use by an upstream or route.
pub const ERROR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
/// 413 — request payload exceeds the size limit.
pub const ERROR_PAYLOAD_TOO_LARGE: &str = "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
/// 429 — rate limit exceeded.
pub const ERROR_RATE_LIMIT_EXCEEDED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// 500 — referenced secret not found.
pub const ERROR_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
/// 502 — protocol-level error.
pub const ERROR_PROTOCOL: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
/// 502 — downstream (upstream service) error.
pub const ERROR_DOWNSTREAM: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
/// 502 — stream connection aborted.
pub const ERROR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
/// 503 — upstream link unavailable.
pub const ERROR_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
/// 503 — circuit breaker open.
pub const ERROR_CIRCUIT_BREAKER_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
/// 503 — plugin not found.
pub const ERROR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
/// 504 — connection timeout.
pub const ERROR_TIMEOUT_CONNECTION: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
/// 504 — request timeout.
pub const ERROR_TIMEOUT_REQUEST: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
/// 504 — idle timeout.
pub const ERROR_TIMEOUT_IDLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";

/// Build an anonymous resource id from a type prefix and a uuid.
#[must_use]
pub fn resource_id(prefix: &str, uuid: uuid::Uuid) -> String {
    format!("{prefix}{uuid}")
}

/// Build an anonymous upstream resource id.
#[must_use]
pub fn upstream_resource_id(uuid: uuid::Uuid) -> String {
    resource_id(UPSTREAM_TYPE_ID, uuid)
}

/// Build an anonymous route resource id.
#[must_use]
pub fn route_resource_id(uuid: uuid::Uuid) -> String {
    resource_id(ROUTE_TYPE_ID, uuid)
}

/// Extract the uuid from a plugin GTS id of the form
/// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`, or `None` when the instance
/// part is not a uuid (i.e. a named plugin).
#[must_use]
pub fn plugin_uuid_from_id(plugin_ref: &str) -> Option<uuid::Uuid> {
    let (_, instance) = plugin_ref.split_once('~')?;
    uuid::Uuid::parse_str(instance).ok()
}

/// Map an error-code stem (e.g. `rate_limit.exceeded`) to its full GTS
/// problem `type` id.
#[must_use]
pub fn error_type(stem: &str) -> String {
    format!("{ERROR_TYPE_PREFIX}{stem}.v1")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn resource_ids_embed_uuid() {
        let uuid = uuid::Uuid::new_v4();
        let id = upstream_resource_id(uuid);
        assert!(id.starts_with(UPSTREAM_TYPE_ID));
        assert_eq!(plugin_uuid_from_id(&id), Some(uuid));
    }

    #[test]
    fn named_plugins_have_no_uuid() {
        assert_eq!(plugin_uuid_from_id(APIKEY_AUTH_PLUGIN_ID), None);
        assert_eq!(plugin_uuid_from_id(REQUIRED_HEADERS_GUARD_PLUGIN_ID), None);
    }

    #[test]
    fn error_type_ids_match_design_table() {
        assert_eq!(ERROR_VALIDATION, error_type("validation.error"));
        assert_eq!(ERROR_RATE_LIMIT_EXCEEDED, error_type("rate_limit.exceeded"));
        assert_eq!(ERROR_ROUTE_NOT_FOUND, error_type("route.not_found"));
        assert_eq!(ERROR_TIMEOUT_REQUEST, error_type("timeout.request"));
    }
}
