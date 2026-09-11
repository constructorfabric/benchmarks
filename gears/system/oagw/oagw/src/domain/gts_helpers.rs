//! GTS identifiers used by the gear (`DESIGN.md` § 3.1, `ADR/0008` § 4,
//! `ADR/0009` § 4).
//!
//! Every identifier is written out in full so the API surface, the persisted
//! model and the registered type catalog all use the same strings.

/// Prefix of every oagw type-schema id.
pub const OAGW_TYPE_PREFIX: &str = "gts.cf.core.oagw.";

/// Type schema for an upstream resource.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";

/// Type schema for a route resource.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";

/// Type schema for an auth plugin.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";

/// Type schema for a guard plugin.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";

/// Type schema for a transform plugin.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Type schema for an upstream protocol.
pub const PROTOCOL_TYPE_ID: &str = "gts.cf.core.oagw.protocol.v1~";

/// Type schema carrying the proxy permission.
pub const PROXY_TYPE_ID: &str = "gts.cf.core.oagw.proxy.v1~";

/// Builds a full plugin instance id from a type schema and an instance part.
#[must_use]
pub fn plugin_id(type_id: &str, instance: &str) -> String {
    format!("{type_id}{instance}")
}

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.<instance>.v1`
#[must_use]
pub fn auth_plugin_id(instance: &str) -> String {
    plugin_id(AUTH_PLUGIN_TYPE_ID, instance)
}

/// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.<instance>.v1`
#[must_use]
pub fn guard_plugin_id(instance: &str) -> String {
    plugin_id(GUARD_PLUGIN_TYPE_ID, instance)
}

/// `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.<instance>.v1`
#[must_use]
pub fn transform_plugin_id(instance: &str) -> String {
    plugin_id(TRANSFORM_PLUGIN_TYPE_ID, instance)
}

/// Anonymous resource id: `gts.cf.core.oagw.<type>.v1~<uuid>`.
#[must_use]
pub fn resource_id(type_id: &str, id: uuid::Uuid) -> String {
    format!("{type_id}{id}")
}

/// Instance id of the `required_headers` guard plugin (`ADR/0009` § 4).
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str = "cf.core.oagw.required_headers.v1";

/// Instance id of the request-id transform plugin.
pub const REQUEST_ID_TRANSFORM_PLUGIN_ID: &str = "cf.core.oagw.request_id.v1";

/// Instance id of the no-op auth plugin.
pub const NOOP_AUTH_PLUGIN_ID: &str = "cf.core.oagw.noop.v1";

/// Instance id of the API-key auth plugin.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "cf.core.oagw.apikey.v1";

/// Instance id of the OAuth2 client-credentials auth plugin (Form auth).
pub const OAUTH2_CLIENT_CRED_PLUGIN_ID: &str = "cf.core.oagw.oauth2_client_cred.v1";

/// Instance id of the OAuth2 client-credentials auth plugin (Basic auth).
pub const OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";

/// Reserved auth plugin identifier with no backing implementation
/// (`cf.core.oagw.basic.v1`).
pub const RESERVED_BASIC_AUTH_PLUGIN_ID: &str = "cf.core.oagw.basic.v1";

/// Reserved auth plugin identifier with no backing implementation
/// (`cf.core.oagw.bearer.v1`).
pub const RESERVED_BEARER_AUTH_PLUGIN_ID: &str = "cf.core.oagw.bearer.v1";

/// Reserved guard identifier for the core timeout policy.
pub const RESERVED_TIMEOUT_GUARD_PLUGIN_ID: &str = "cf.core.oagw.timeout.v1";

/// Reserved guard identifier for the core CORS policy.
pub const RESERVED_CORS_GUARD_PLUGIN_ID: &str = "cf.core.oagw.cors.v1";

/// Reserved transform identifier for core request/response logging.
pub const RESERVED_LOGGING_TRANSFORM_PLUGIN_ID: &str = "cf.core.oagw.logging.v1";

/// Reserved transform identifier for core metrics collection.
pub const RESERVED_METRICS_TRANSFORM_PLUGIN_ID: &str = "cf.core.oagw.metrics.v1";

/// Full GTS id of the built-in `noop` auth plugin.
pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";

/// Full GTS id of the built-in `apikey` auth plugin.
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Full GTS id of the built-in `oauth2_client_cred` auth plugin.
pub const AUTH_OAUTH2: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// Full GTS id of the built-in `oauth2_client_cred_basic` auth plugin.
pub const AUTH_OAUTH2_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Full GTS id of the built-in `required_headers` guard plugin.
pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// Full GTS id of the built-in `request_id` transform plugin.
pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Every type-schema id the gear declares to the types-registry.
pub const TYPE_SCHEMA_IDS: &[&str] = &[
    UPSTREAM_TYPE_ID,
    ROUTE_TYPE_ID,
    AUTH_PLUGIN_TYPE_ID,
    GUARD_PLUGIN_TYPE_ID,
    TRANSFORM_PLUGIN_TYPE_ID,
    PROTOCOL_TYPE_ID,
    PROXY_TYPE_ID,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_ids_are_fully_qualified() {
        assert_eq!(AUTH_NOOP, auth_plugin_id(NOOP_AUTH_PLUGIN_ID));
        assert_eq!(AUTH_APIKEY, auth_plugin_id(APIKEY_AUTH_PLUGIN_ID));
        assert_eq!(
            GUARD_REQUIRED_HEADERS,
            guard_plugin_id(REQUIRED_HEADERS_GUARD_PLUGIN_ID)
        );
        assert_eq!(
            TRANSFORM_REQUEST_ID,
            transform_plugin_id(REQUEST_ID_TRANSFORM_PLUGIN_ID)
        );
        assert_eq!(
            auth_plugin_id(RESERVED_BASIC_AUTH_PLUGIN_ID),
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"
        );
    }

    #[test]
    fn resource_ids_use_the_anonymous_form() {
        let id = uuid::Uuid::nil();
        assert_eq!(
            resource_id(UPSTREAM_TYPE_ID, id),
            "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000000"
        );
    }
}
