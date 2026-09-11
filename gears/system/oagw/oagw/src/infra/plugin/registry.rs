//! In-process plugin registries.
//!
//! Named (built-in) plugins live here and are never persisted; UUID-backed
//! custom plugins are stored in `oagw_plugin` instead
//! (DESIGN "Plugin Identification Model").

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, PluginCatalog, TransformPlugin};
use crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin;
use crate::infra::plugin::noop_auth::NoopAuthPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::{
    OAuth2ClientCredAuthPlugin, TokenCacheConfig,
};
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;

#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache_config: TokenCacheConfig,
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
                token_cache_config.ttl,
                token_cache_config.capacity,
            )),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                credstore,
                ClientAuthMethod::Basic,
                token_cache_config.ttl,
                token_cache_config.capacity,
            )),
        );
        Self { plugins }
    }

    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(id).cloned()
    }

    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.plugins.keys().map(String::as_str).collect()
    }
}

#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        plugins.insert(
            REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(id).cloned()
    }
}

#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        plugins.insert(
            REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(id).cloned()
    }
}

/// The three registries, shared by the Control Plane (for write-time
/// validation) and the Data Plane (for execution).
pub struct PluginRegistries {
    pub auth: AuthPluginRegistry,
    pub guard: GuardPluginRegistry,
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(credstore, token_cache_config),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }
}

impl PluginCatalog for PluginRegistries {
    fn has_auth(&self, id: &str) -> bool {
        self.auth.get(id).is_some()
    }

    fn has_guard(&self, id: &str) -> bool {
        self.guard.get(id).is_some()
    }

    fn has_transform(&self, id: &str) -> bool {
        self.transform.get(id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::{
        BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID, CORS_GUARD_PLUGIN_ID,
        LOGGING_TRANSFORM_PLUGIN_ID, METRICS_TRANSFORM_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID,
    };

    fn registries() -> PluginRegistries {
        let credstore: Arc<dyn CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        PluginRegistries::with_builtins(credstore, TokenCacheConfig::default())
    }

    #[test]
    fn all_four_auth_plugins_resolve() {
        let r = registries();
        for id in [
            NOOP_AUTH_PLUGIN_ID,
            APIKEY_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        ] {
            assert!(r.has_auth(id), "{id} should resolve");
        }
    }

    #[test]
    fn catalog_only_identifiers_do_not_resolve() {
        let r = registries();
        // basic / bearer are cataloged but have no backing AuthPlugin.
        assert!(!r.has_auth(BASIC_AUTH_PLUGIN_ID));
        assert!(!r.has_auth(BEARER_AUTH_PLUGIN_ID));
        // timeout / cors are core Data Plane logic, not GuardPlugins.
        assert!(!r.has_guard(TIMEOUT_GUARD_PLUGIN_ID));
        assert!(!r.has_guard(CORS_GUARD_PLUGIN_ID));
        // logging / metrics are core instrumentation, not TransformPlugins.
        assert!(!r.has_transform(LOGGING_TRANSFORM_PLUGIN_ID));
        assert!(!r.has_transform(METRICS_TRANSFORM_PLUGIN_ID));
    }

    #[test]
    fn required_headers_is_the_only_builtin_guard() {
        let r = registries();
        assert!(r.has_guard(REQUIRED_HEADERS_GUARD_PLUGIN_ID));
        assert!(r.has_transform(REQUEST_ID_TRANSFORM_PLUGIN_ID));
    }
}
