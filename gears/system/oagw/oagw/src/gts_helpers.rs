//! GTS identifier constants for the OAGW gear.
//!
//! Error types follow the catalog in `docs/DESIGN.md` § Error Response
//! Format: `gts.cf.core.errors.err.v1~cf.oagw.<...>.v1`. Resource and plugin
//! types follow the control-plane identification model in `DESIGN.md`.

// ---------------------------------------------------------------------------
// Error type identifiers (RFC 9457 `type` field)
// ---------------------------------------------------------------------------

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
pub const ERR_CONNECTION_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
pub const ERR_REQUEST_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
pub const ERR_IDLE_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";

pub const ERR_REQUIRED_HEADER_MISSING: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.guard.required_header_missing.v1";

pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

// ---------------------------------------------------------------------------
// Resource type identifiers
// ---------------------------------------------------------------------------

pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1";
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1";
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1";
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1";
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1";

pub const PROTOCOL_HTTP_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
pub const PROTOCOL_GRPC_TYPE: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

// ---------------------------------------------------------------------------
// Built-in plugin identifiers
// ---------------------------------------------------------------------------

pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
pub const BASIC_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
pub const BEARER_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

// ---------------------------------------------------------------------------
// Permissions (DESIGN.md § Permissions and Access Control)
// ---------------------------------------------------------------------------

pub const PERM_UPSTREAM_CREATE: &str = "gts.cf.core.oagw.upstream.v1~:create";
pub const PERM_UPSTREAM_OVERRIDE: &str = "gts.cf.core.oagw.upstream.v1~:override";
pub const PERM_UPSTREAM_READ: &str = "gts.cf.core.oagw.upstream.v1~:read";
pub const PERM_UPSTREAM_DELETE: &str = "gts.cf.core.oagw.upstream.v1~:delete";
pub const PERM_ROUTE_CREATE: &str = "gts.cf.core.oagw.route.v1~:create";
pub const PERM_ROUTE_OVERRIDE: &str = "gts.cf.core.oagw.route.v1~:override";
pub const PERM_ROUTE_READ: &str = "gts.cf.core.oagw.route.v1~:read";
pub const PERM_ROUTE_DELETE: &str = "gts.cf.core.oagw.route.v1~:delete";
pub const PERM_AUTH_PLUGIN_CREATE: &str = "gts.cf.core.oagw.auth_plugin.v1~:create";
pub const PERM_AUTH_PLUGIN_READ: &str = "gts.cf.core.oagw.auth_plugin.v1~:read";
pub const PERM_AUTH_PLUGIN_DELETE: &str = "gts.cf.core.oagw.auth_plugin.v1~:delete";
pub const PERM_GUARD_PLUGIN_CREATE: &str = "gts.cf.core.oagw.guard_plugin.v1~:create";
pub const PERM_GUARD_PLUGIN_READ: &str = "gts.cf.core.oagw.guard_plugin.v1~:read";
pub const PERM_GUARD_PLUGIN_DELETE: &str = "gts.cf.core.oagw.guard_plugin.v1~:delete";
pub const PERM_TRANSFORM_PLUGIN_CREATE: &str = "gts.cf.core.oagw.transform_plugin.v1~:create";
pub const PERM_TRANSFORM_PLUGIN_READ: &str = "gts.cf.core.oagw.transform_plugin.v1~:read";
pub const PERM_TRANSFORM_PLUGIN_DELETE: &str = "gts.cf.core.oagw.transform_plugin.v1~:delete";
pub const PERM_PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

/// True when the given scope list grants unrestricted access (e2e static
/// authn tokens carry `["*"]`).
#[must_use]
pub fn has_permission(scopes: &[String], permission: &str) -> bool {
    scopes.iter().any(|s| s == "*" || s == permission)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn wildcard_scope_grants_everything() {
        assert!(has_permission(&["*".to_owned()], PERM_PROXY_INVOKE));
        assert!(has_permission(&["*".to_owned()], PERM_UPSTREAM_DELETE));
    }

    #[test]
    fn explicit_scope_grants_only_itself() {
        let scopes = vec![PERM_UPSTREAM_READ.to_owned()];
        assert!(has_permission(&scopes, PERM_UPSTREAM_READ));
        assert!(!has_permission(&scopes, PERM_UPSTREAM_CREATE));
        assert!(!has_permission(&scopes, PERM_PROXY_INVOKE));
    }

    #[test]
    fn empty_scopes_grant_nothing() {
        assert!(!has_permission(&[], PERM_PROXY_INVOKE));
    }
}
