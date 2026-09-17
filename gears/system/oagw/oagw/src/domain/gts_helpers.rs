//! Every GTS identifier the OAGW API emits.
//!
//! Values mirror the design documents exactly; tests in
//! `crate::domain::tests` assert the literal strings so a rename here cannot
//! silently change the wire contract.

/// Prefix shared by every OAGW entity type identifier.
pub const OAGW_TYPE_PREFIX: &str = "gts.cf.core.oagw";

/// Error family prefix (`gts.cf.core.errors.err.v1~cf.oagw.…`).
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw";

/// `gts.cf.core.oagw.upstream.v1~`
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// `gts.cf.core.oagw.route.v1~`
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";
/// `gts.cf.core.oagw.plugin.v1~`
pub const PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.plugin.v1~";
/// `gts.cf.core.oagw.proxy.v1~`
pub const PROXY_TYPE_ID: &str = "gts.cf.core.oagw.proxy.v1~";

/// HTTP upstream protocol.
pub const PROTOCOL_HTTP_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC upstream protocol.
pub const PROTOCOL_GRPC_ID: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// No-op auth plugin.
pub const NOOP_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// API-key auth plugin.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// OAuth2 client-credentials auth plugin (form-encoded client auth).
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// OAuth2 client-credentials auth plugin (Basic client auth).
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Required-headers guard plugin.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Request-id transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Catalogued Starlark plugin kinds (registered as names, never implemented).
pub const CATALOG_ONLY_PLUGIN_IDS: &[&str] = &[
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
];

/// Management API permission base for upstreams.
pub const PERM_UPSTREAM: &str = "gts.cf.core.oagw.upstream.v1~";
/// Management API permission base for routes.
pub const PERM_ROUTE: &str = "gts.cf.core.oagw.route.v1~";
/// Proxy invoke permission.
pub const PERM_PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

/// `X-OAGW-Error-Source: gateway`
pub const ERROR_SOURCE_HEADER: &str = "X-OAGW-Error-Source";
/// `X-OAGW-Target-Host`
pub const TARGET_HOST_HEADER: &str = "X-OAGW-Target-Host";
/// `X-OAGW-Request-Id`
pub const REQUEST_ID_HEADER: &str = "X-OAGW-Request-Id";

/// Builds an entity instance identifier (`…v1~{uuid}`).
#[must_use]
pub fn instance_id(type_prefix: &str, id: &uuid::Uuid) -> String {
    format!("{type_prefix}{id}")
}

/// Builds an error `type` value from its OAGW suffix (e.g. `route.not_found`).
#[must_use]
pub fn error_type(suffix: &str) -> String {
    format!("{ERROR_TYPE_PREFIX}.{suffix}.v1")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn error_types_match_design_table() {
        assert_eq!(
            error_type("validation.error"),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(
            error_type("routing.missing_target_host"),
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
        assert_eq!(
            error_type("timeout.request"),
            "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1"
        );
    }

    #[test]
    fn protocol_and_plugin_ids_match_design() {
        assert_eq!(
            PROTOCOL_HTTP_ID,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
        assert_eq!(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
        );
        assert_eq!(
            REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
    }
}
