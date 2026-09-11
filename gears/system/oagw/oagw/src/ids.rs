//! GTS identifiers and well-known names used across the gear.
//!
//! Every identifier is spelled out through [`toolkit_gts::gts_id`] so the
//! compile-time validator rejects a malformed id instead of letting a typo
//! reach the wire.

use toolkit_gts::gts_id;

/// GTS type id of the upstream definition resource.
pub const UPSTREAM_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// GTS type id of the route definition resource.
pub const ROUTE_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// GTS type id of the proxy invocation resource.
pub const PROXY_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.proxy.v1~");
/// GTS type id of the auth-plugin definition resource.
pub const AUTH_PLUGIN_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// GTS type id of the guard-plugin definition resource.
pub const GUARD_PLUGIN_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// GTS type id of the transform-plugin definition resource.
pub const TRANSFORM_PLUGIN_RESOURCE_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

/// Prefix every OAGW problem `type` id shares.
///
/// A problem `type` is a *complete* GTS id —
/// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1` — so this constant
/// is the shared head of those ids rather than an id of its own. The suffix
/// each [`crate::error::ErrorKind`] appends is the `cf.oagw.`-relative tail the
/// specification lists; [`EXAMPLE_PROBLEM_TYPE`] is a complete id spelled
/// through [`gts_id!`], and the test below pins the two to each other.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw.";
/// A complete problem `type` id, validated at compile time; the test module
/// below pins it to [`ERROR_TYPE_PREFIX`].
#[cfg(test)]
const EXAMPLE_PROBLEM_TYPE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
/// Protocol id of the (implemented) HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// Protocol id of the gRPC upstream protocol (accepted, not yet proxied).
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// `X-OAGW-Target-Host` — the client's endpoint-selection hint.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";
/// `X-OAGW-Error-Source` — `gateway` | `upstream`.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// Value of [`ERROR_SOURCE_HEADER`] for errors OAGW produced.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// Value of [`ERROR_SOURCE_HEADER`] for errors passed through from upstream.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// Auth plugin id of the no-op (anonymous) authentication plugin.
pub const AUTH_PLUGIN_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
/// Auth plugin id of the API-key injection plugin.
pub const AUTH_PLUGIN_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
/// Auth plugin id of the OAuth2 client-credentials (form body) plugin.
pub const AUTH_PLUGIN_OAUTH2_CC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
/// Auth plugin id of the OAuth2 client-credentials (Basic auth) plugin.
pub const AUTH_PLUGIN_OAUTH2_CC_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// Auth plugin id of the HTTP Basic plugin — catalog entry only.
pub const AUTH_PLUGIN_BASIC: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Auth plugin id of the bearer plugin — catalog entry only.
pub const AUTH_PLUGIN_BEARER: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

/// Guard plugin id of the required-headers guard (the only bindable guard).
pub const GUARD_PLUGIN_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Guard plugin id of the request timeout — catalog entry only.
pub const GUARD_PLUGIN_TIMEOUT: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// Guard plugin id of CORS — catalog entry only.
pub const GUARD_PLUGIN_CORS: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

/// Transform plugin id of `X-Request-ID` propagation.
pub const TRANSFORM_PLUGIN_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Transform plugin id of request logging — catalog entry only.
pub const TRANSFORM_PLUGIN_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Transform plugin id of metrics collection — catalog entry only.
pub const TRANSFORM_PLUGIN_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// The auth plugin identifiers that carry a real `AuthPlugin` implementation.
pub const IMPLEMENTED_AUTH_PLUGINS: [&str; 4] = [
    AUTH_PLUGIN_NOOP,
    AUTH_PLUGIN_APIKEY,
    AUTH_PLUGIN_OAUTH2_CC,
    AUTH_PLUGIN_OAUTH2_CC_BASIC,
];

/// Auth plugin identifiers that exist only as catalog entries.
pub const CATALOG_ONLY_AUTH_PLUGINS: [&str; 2] = [AUTH_PLUGIN_BASIC, AUTH_PLUGIN_BEARER];

/// Guard plugin identifiers that cannot be bound through `plugins.items[]`.
pub const CATALOG_ONLY_GUARD_PLUGINS: [&str; 2] = [GUARD_PLUGIN_TIMEOUT, GUARD_PLUGIN_CORS];

/// Transform plugin identifiers that are not `TransformPluginRegistry`-resolvable.
pub const CATALOG_ONLY_TRANSFORM_PLUGINS: [&str; 2] =
    [TRANSFORM_PLUGIN_LOGGING, TRANSFORM_PLUGIN_METRICS];

/// The built-in plugin identifiers that carry an implementation and can
/// therefore be bound through `auth.type` or `plugins.items[].plugin_ref`.
pub const BINDABLE_PLUGIN_IDS: [&str; 6] = [
    AUTH_PLUGIN_NOOP,
    AUTH_PLUGIN_APIKEY,
    AUTH_PLUGIN_OAUTH2_CC,
    AUTH_PLUGIN_OAUTH2_CC_BASIC,
    GUARD_PLUGIN_REQUIRED_HEADERS,
    TRANSFORM_PLUGIN_REQUEST_ID,
];

/// Every built-in plugin identifier the gear publishes, across all types.
pub fn builtin_plugin_ids() -> Vec<&'static str> {
    [
        AUTH_PLUGIN_NOOP,
        AUTH_PLUGIN_APIKEY,
        AUTH_PLUGIN_OAUTH2_CC,
        AUTH_PLUGIN_OAUTH2_CC_BASIC,
        AUTH_PLUGIN_BASIC,
        AUTH_PLUGIN_BEARER,
        GUARD_PLUGIN_REQUIRED_HEADERS,
        GUARD_PLUGIN_TIMEOUT,
        GUARD_PLUGIN_CORS,
        TRANSFORM_PLUGIN_REQUEST_ID,
        TRANSFORM_PLUGIN_LOGGING,
        TRANSFORM_PLUGIN_METRICS,
    ]
    .into_iter()
    .collect()
}

/// The plugin type an identifier belongs to, or `None` when it is unknown.
#[must_use]
pub fn plugin_type_of(plugin_ref: &str) -> Option<&'static str> {
    let prefix = match () {
        () if plugin_ref.starts_with(AUTH_PLUGIN_RESOURCE_TYPE) => AUTH_PLUGIN_RESOURCE_TYPE,
        () if plugin_ref.starts_with(GUARD_PLUGIN_RESOURCE_TYPE) => GUARD_PLUGIN_RESOURCE_TYPE,
        () if plugin_ref.starts_with(TRANSFORM_PLUGIN_RESOURCE_TYPE) => {
            TRANSFORM_PLUGIN_RESOURCE_TYPE
        }
        () => return None,
    };
    Some(prefix)
}

/// `true` when `plugin_ref` names a built-in plugin of `plugin_type`.
#[must_use]
pub fn is_builtin(plugin_ref: &str) -> bool {
    builtin_plugin_ids().contains(&plugin_ref)
}

/// `true` when `plugin_ref` names a UUID-backed custom plugin.
#[must_use]
pub fn is_custom_ref(plugin_ref: &str) -> bool {
    !is_builtin(plugin_ref)
        && plugin_ref
            .rsplit('~')
            .next()
            .is_some_and(|instance| uuid::Uuid::parse_str(instance).is_ok())
}

/// `true` when `plugin_ref` can be bound to an upstream or a route.
///
/// Built-ins that are catalog entries only (`basic`, `bearer`, `timeout`,
/// `cors`, `logging`, `metrics`) are recognized but not bindable; a custom
/// plugin reference is bindable when the control plane holds its definition.
#[must_use]
pub fn is_bindable(plugin_ref: &str) -> bool {
    BINDABLE_PLUGIN_IDS.contains(&plugin_ref) || is_custom_ref(plugin_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_problem_type_prefix_is_the_head_of_a_valid_gts_id() {
        assert!(EXAMPLE_PROBLEM_TYPE.starts_with(ERROR_TYPE_PREFIX));
        assert_eq!(
            format!(
                "{ERROR_TYPE_PREFIX}{}",
                crate::error::ErrorKind::RateLimit.gts_type()
            ),
            "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1"
        );
    }

    #[test]
    fn the_resource_type_ids_end_in_the_instance_separator() {
        for id in [
            UPSTREAM_RESOURCE_TYPE,
            ROUTE_RESOURCE_TYPE,
            AUTH_PLUGIN_RESOURCE_TYPE,
            GUARD_PLUGIN_RESOURCE_TYPE,
            TRANSFORM_PLUGIN_RESOURCE_TYPE,
        ] {
            assert!(id.ends_with('~'), "{id} is a GTS type id");
        }
    }

    #[test]
    fn every_builtin_plugin_id_carries_a_type_and_an_instance() {
        for id in builtin_plugin_ids() {
            assert!(id.contains('~'), "{id} must be a `type~instance` id");
            let (type_id, instance) = id.split_once('~').expect("split");
            assert!(type_id.ends_with(".v1"), "{type_id} is a type id");
            assert!(!instance.is_empty(), "{id} names an instance");
        }
    }

    #[test]
    fn the_built_in_plugin_ids_match_the_specification() {
        assert_eq!(
            AUTH_PLUGIN_NOOP,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
        );
        assert_eq!(
            AUTH_PLUGIN_APIKEY,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        );
        assert_eq!(
            AUTH_PLUGIN_OAUTH2_CC_BASIC,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
        );
        assert_eq!(
            GUARD_PLUGIN_REQUIRED_HEADERS,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert_eq!(
            TRANSFORM_PLUGIN_REQUEST_ID,
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        );
        assert_eq!(
            PROTOCOL_HTTP,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
    }
}
