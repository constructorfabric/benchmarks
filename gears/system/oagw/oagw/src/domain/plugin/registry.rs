//! Built-in plugin identifiers and the real [`NamedPluginRegistry`]
//! implementation backed by them
//! (`cpt-cf-oagw-dod-runtime-plugin-resolution`,
//! `cpt-cf-oagw-dod-plugin-kinds`).
//!
//! Feature 4 (`cpt-cf-oagw-feature-plugin-management`) shipped only
//! [`crate::domain::plugin_resolve::EmptyNamedPluginRegistry`], since no
//! built-in plugin behavior existed yet. This module is the first to
//! install a registry that actually matches the six identifiers this
//! feature gives real behavior to; the six catalogue-only identifiers named
//! in `cpt-cf-oagw-algo-runtime-plugin-resolution` (`basic`, `bearer`,
//! `timeout`, `cors`, `logging`, `metrics`) are deliberately absent, so
//! [`resolve_plugin_ref`](crate::domain::plugin_resolve::resolve_plugin_ref)
//! reports them unresolved.

use crate::domain::model::PluginType;
use crate::domain::plugin_resolve::NamedPluginRegistry;

/// The no-operation auth plugin's instance name.
pub const NOOP_AUTH_PLUGIN_NAME: &str = "cf.core.oagw.noop.v1";
/// The api-key auth plugin's instance name.
pub const APIKEY_AUTH_PLUGIN_NAME: &str = "cf.core.oagw.apikey.v1";
/// The `Form` client-authentication `OAuth2` client-credentials plugin's
/// instance name.
pub const OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME: &str = "cf.core.oagw.oauth2_client_cred.v1";
/// The `Basic` client-authentication `OAuth2` client-credentials plugin's
/// instance name.
pub const OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";
/// The required-headers guard plugin's instance name.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_NAME: &str = "cf.core.oagw.required_headers.v1";
/// The request-id transform plugin's instance name.
pub const REQUEST_ID_TRANSFORM_PLUGIN_NAME: &str = "cf.core.oagw.request_id.v1";

/// The real, populated [`NamedPluginRegistry`]: `true` for exactly the six
/// built-in identifiers this feature implements, `false` for everything
/// else — including the six catalogue-only identifiers and any unknown name
/// (`cpt-cf-oagw-dod-runtime-plugin-resolution`).
#[derive(Debug, Default, Clone, Copy)]
pub struct BuiltinPluginRegistry;

impl NamedPluginRegistry for BuiltinPluginRegistry {
    fn is_registered(&self, plugin_type: PluginType, name: &str) -> bool {
        match plugin_type {
            PluginType::Auth => matches!(
                name,
                NOOP_AUTH_PLUGIN_NAME
                    | APIKEY_AUTH_PLUGIN_NAME
                    | OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME
                    | OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME
            ),
            PluginType::Guard => name == REQUIRED_HEADERS_GUARD_PLUGIN_NAME,
            PluginType::Transform => name == REQUEST_ID_TRANSFORM_PLUGIN_NAME,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        APIKEY_AUTH_PLUGIN_NAME, BuiltinPluginRegistry, NOOP_AUTH_PLUGIN_NAME,
        OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME, OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME,
        REQUEST_ID_TRANSFORM_PLUGIN_NAME, REQUIRED_HEADERS_GUARD_PLUGIN_NAME,
    };
    use crate::domain::model::PluginType;
    use crate::domain::plugin_resolve::NamedPluginRegistry;

    // @cpt-begin:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-builtin-registry-test-01
    #[test]
    fn every_built_in_identifier_is_registered_for_its_kind() {
        let registry = BuiltinPluginRegistry;
        assert!(registry.is_registered(PluginType::Auth, NOOP_AUTH_PLUGIN_NAME));
        assert!(registry.is_registered(PluginType::Auth, APIKEY_AUTH_PLUGIN_NAME));
        assert!(registry.is_registered(PluginType::Auth, OAUTH2_CLIENT_CRED_FORM_PLUGIN_NAME));
        assert!(registry.is_registered(PluginType::Auth, OAUTH2_CLIENT_CRED_BASIC_PLUGIN_NAME));
        assert!(registry.is_registered(PluginType::Guard, REQUIRED_HEADERS_GUARD_PLUGIN_NAME));
        assert!(registry.is_registered(PluginType::Transform, REQUEST_ID_TRANSFORM_PLUGIN_NAME));
    }
    // @cpt-end:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-builtin-registry-test-01

    // @cpt-begin:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-builtin-registry-catalogue-test-01
    #[test]
    fn catalogue_only_identifiers_are_never_registered() {
        let registry = BuiltinPluginRegistry;
        assert!(!registry.is_registered(PluginType::Auth, "cf.core.oagw.basic.v1"));
        assert!(!registry.is_registered(PluginType::Auth, "cf.core.oagw.bearer.v1"));
        assert!(!registry.is_registered(PluginType::Guard, "cf.core.oagw.timeout.v1"));
        assert!(!registry.is_registered(PluginType::Guard, "cf.core.oagw.cors.v1"));
        assert!(!registry.is_registered(PluginType::Transform, "cf.core.oagw.logging.v1"));
        assert!(!registry.is_registered(PluginType::Transform, "cf.core.oagw.metrics.v1"));
    }
    // @cpt-end:cpt-cf-oagw-dod-runtime-plugin-resolution:p2:inst-builtin-registry-catalogue-test-01

    #[test]
    fn a_name_registered_under_a_different_kind_does_not_match() {
        let registry = BuiltinPluginRegistry;
        assert!(!registry.is_registered(PluginType::Guard, APIKEY_AUTH_PLUGIN_NAME));
        assert!(!registry.is_registered(PluginType::Transform, REQUIRED_HEADERS_GUARD_PLUGIN_NAME));
    }
}
