//! GTS identifiers used by the `oagw` gear.
//!
//! Every error carries a `gts.cf.core.errors.err.v1~cf.oagw.<family>.<name>.v1`
//! identifier; every configuration object references the protocol and plugin
//! identifiers tabulated here.

use uuid::Uuid;

/// Prefix of every gateway-generated error type identifier.
pub const ERROR_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw";

/// Builds the error type identifier for `family.name`.
#[must_use]
pub fn error_id(family: &str, name: &str) -> String {
    format!("{ERROR_PREFIX}.{family}.{name}.v1")
}

/// Identifier of the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Identifier of the gRPC upstream protocol.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Type-schema identifier of the upstream resource.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";

/// Type-schema identifier of the route resource.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";

/// Type-schema identifier of the auth-plugin family.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";

/// Type-schema identifier of the guard-plugin family.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";

/// Type-schema identifier of the transform-plugin family.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Identifier of the built-in API-key authentication plugin.
pub const AUTH_PLUGIN_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Identifier of the built-in `OAuth2` client-credentials authentication plugin.
pub const AUTH_PLUGIN_OAUTH2_CLIENT_CRED: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// Identifier of the built-in no-op authentication plugin.
pub const AUTH_PLUGIN_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";

/// Identifier of the `OAuth2` client-credentials plugin using `Basic` client auth.
pub const AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Identifier of the built-in required-headers guard plugin.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Identifier of the built-in request-id transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// The `basic` auth identifier: cataloged, never resolvable.
pub const CATALOG_ONLY_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";

/// The `bearer` auth identifier: cataloged, never resolvable.
pub const CATALOG_ONLY_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// The `timeout` guard identifier: cataloged, never bindable.
pub const CATALOG_ONLY_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";

/// The `cors` guard identifier: cataloged, never bindable.
pub const CATALOG_ONLY_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// The `logging` transform identifier: cataloged, never resolvable.
pub const CATALOG_ONLY_LOGGING: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";

/// The `metrics` transform identifier: cataloged, never resolvable.
pub const CATALOG_ONLY_METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Plugin identifiers present in the GTS type catalog but with no runtime
/// implementation: resolving one must fail closed.
pub const CATALOG_ONLY_PLUGIN_IDS: [&str; 6] = [
    CATALOG_ONLY_BASIC,
    CATALOG_ONLY_BEARER,
    CATALOG_ONLY_TIMEOUT,
    CATALOG_ONLY_CORS,
    CATALOG_ONLY_LOGGING,
    CATALOG_ONLY_METRICS,
];

/// Returns `true` when `id` is registered in the catalog but has no runtime
/// plugin implementation.
#[must_use]
pub fn is_catalog_only_plugin(id: &str) -> bool {
    CATALOG_ONLY_PLUGIN_IDS.contains(&id)
}

/// The UUID a plugin identifier is backed by, if it has one.
///
/// A custom plugin's instance part — everything after `~` — is the UUID the
/// control plane minted for it; a named plugin's instance part is a name.
#[must_use]
pub fn plugin_uuid_of(reference: &str) -> Option<Uuid> {
    let instance = reference.split('~').next_back()?;
    Uuid::parse_str(instance).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_ids_are_canonical() {
        assert_eq!(
            error_id("route", "not_found"),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[test]
    fn basic_and_bearer_are_catalog_only() {
        assert!(is_catalog_only_plugin(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"
        ));
        assert!(!is_catalog_only_plugin(AUTH_PLUGIN_APIKEY));
    }
}
