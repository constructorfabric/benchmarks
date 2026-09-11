//! In-process plugin registries.
//!
//! Named plugins live here and are never persisted; UUID-backed custom
//! plugins live in `oagw_plugin` and are resolved by the Control Plane. The
//! resolution algorithm is `docs/DESIGN.md` §"Resolution Algorithm".

use credstore_sdk::CredStoreClientV1;
use std::collections::HashMap;
use std::sync::Arc;
use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

use super::apikey_auth::ApiKeyAuthPlugin;
use super::noop_auth::NoopAuthPlugin;
use super::oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
use super::request_id_transform::RequestIdTransformPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;

/// Named auth plugins.
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Register the built-ins listed in `cpt-cf-oagw-fr-builtin-plugins`.
    ///
    /// `basic.v1` and `bearer.v1` are deliberately absent: they are reserved
    /// catalog identifiers with no backing implementation, so binding one
    /// must fail with `unknown auth plugin`.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        let mut plugins: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        plugins.insert(NOOP_AUTH_PLUGIN_ID.to_owned(), Arc::new(NoopAuthPlugin));
        plugins.insert(
            APIKEY_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&credstore))),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                Arc::clone(&credstore),
                ClientAuthMethod::Form,
                token_cache.ttl,
                token_cache.capacity,
            )),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                credstore,
                ClientAuthMethod::Basic,
                token_cache.ttl,
                token_cache.capacity,
            )),
        );
        Self { plugins }
    }

    /// Resolve a named auth plugin.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(plugin_ref).map(Arc::clone)
    }

    /// Registered identifiers, for diagnostics.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

/// Named guard plugins.
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Register the built-ins.
    ///
    /// `timeout.v1` and `cors.v1` are catalog-only: both are core Data Plane
    /// logic, not `GuardPlugin` implementations.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        plugins.insert(
            REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    /// Resolve a named guard plugin.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_ref).map(Arc::clone)
    }

    /// Registered identifiers, for diagnostics.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

impl Default for GuardPluginRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// Named transform plugins.
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Register the built-ins.
    ///
    /// `logging.v1` and `metrics.v1` are catalog-only: both are core Data
    /// Plane instrumentation.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        plugins.insert(
            REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    /// Resolve a named transform plugin.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_ref).map(Arc::clone)
    }

    /// Registered identifiers, for diagnostics.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

impl Default for TransformPluginRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// The three registries, handed to the Data Plane as one unit.
pub struct PluginRegistries {
    /// Named auth plugins.
    pub auth: AuthPluginRegistry,
    /// Named guard plugins.
    pub guard: GuardPluginRegistry,
    /// Named transform plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Build all three with their built-ins registered.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(credstore, token_cache),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::{
        BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID, CORS_GUARD_PLUGIN_ID,
        LOGGING_TRANSFORM_PLUGIN_ID, METRICS_TRANSFORM_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID,
    };
    use crate::infra::plugin::test_support::mock_credstore;

    fn registries() -> PluginRegistries {
        PluginRegistries::with_builtins(mock_credstore(Vec::new()), TokenCacheConfig::default())
    }

    #[test]
    fn every_documented_builtin_resolves() {
        let reg = registries();
        for id in [
            NOOP_AUTH_PLUGIN_ID,
            APIKEY_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        ] {
            assert!(reg.auth.get(id).is_some(), "{id} should resolve");
        }
        assert!(reg.guard.get(REQUIRED_HEADERS_GUARD_PLUGIN_ID).is_some());
        assert!(reg.transform.get(REQUEST_ID_TRANSFORM_PLUGIN_ID).is_some());
    }

    #[test]
    fn catalog_only_identifiers_do_not_resolve() {
        let reg = registries();
        assert!(reg.auth.get(BASIC_AUTH_PLUGIN_ID).is_none());
        assert!(reg.auth.get(BEARER_AUTH_PLUGIN_ID).is_none());
        assert!(reg.guard.get(TIMEOUT_GUARD_PLUGIN_ID).is_none());
        assert!(reg.guard.get(CORS_GUARD_PLUGIN_ID).is_none());
        assert!(reg.transform.get(LOGGING_TRANSFORM_PLUGIN_ID).is_none());
        assert!(reg.transform.get(METRICS_TRANSFORM_PLUGIN_ID).is_none());
    }

    #[test]
    fn required_headers_is_the_only_registered_guard() {
        assert_eq!(GuardPluginRegistry::with_builtins().ids().len(), 1);
    }
}
