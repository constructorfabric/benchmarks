//! In-process plugin registries.
//!
//! Named plugins — the built-ins, and anything a deployed gear contributes —
//! are resolved here and are never persisted (`DESIGN.md` § *Plugin
//! Identification Model*). A registry deliberately does **not** contain the
//! catalog-only identifiers: `basic`/`bearer` have no backing implementation,
//! and `timeout`/`cors`/`logging`/`metrics` are core Data Plane behaviour, so
//! binding any of them fails to resolve rather than silently doing nothing.

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::ClientAuthMethod;

use crate::config::TokenCacheConfig;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

use super::apikey_auth::ApiKeyAuthPlugin;
use super::noop_auth::NoopAuthPlugin;
use super::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin;
use super::request_id_transform::RequestIdTransformPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;

/// Auth plugins, keyed by GTS identifier.
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Register the built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_http_config: Option<toolkit_http::HttpClientConfig>,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        let mut plugins: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        let mut register = |plugin: Arc<dyn AuthPlugin>| {
            plugins.insert(plugin.plugin_type().to_owned(), plugin);
        };
        register(Arc::new(NoopAuthPlugin));
        register(Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&credstore))));
        register(Arc::new(
            OAuth2ClientCredAuthPlugin::new(
                Arc::clone(&credstore),
                ClientAuthMethod::Form,
                token_cache_config.ttl,
                token_cache_config.capacity,
            )
            .with_http_config(token_http_config.clone()),
        ));
        register(Arc::new(
            OAuth2ClientCredAuthPlugin::new(
                credstore,
                ClientAuthMethod::Basic,
                token_cache_config.ttl,
                token_cache_config.capacity,
            )
            .with_http_config(token_http_config),
        ));
        Self { plugins }
    }

    /// An empty registry (external plugins only).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: HashMap::new(),
        }
    }

    /// Add an externally-provided plugin.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, plugin_type: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(plugin_type).map(Arc::clone)
    }

    /// Whether `plugin_type` resolves.
    #[must_use]
    pub fn contains(&self, plugin_type: &str) -> bool {
        self.plugins.contains_key(plugin_type)
    }

    /// Registered identifiers, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.plugins.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }
}

/// Guard plugins, keyed by GTS identifier.
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Register the built-in guard plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        let plugin: Arc<dyn GuardPlugin> = Arc::new(RequiredHeadersGuardPlugin);
        plugins.insert(plugin.plugin_type().to_owned(), plugin);
        Self { plugins }
    }

    /// Add an externally-provided plugin.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, plugin_type: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_type).map(Arc::clone)
    }

    /// Whether `plugin_type` resolves.
    #[must_use]
    pub fn contains(&self, plugin_type: &str) -> bool {
        self.plugins.contains_key(plugin_type)
    }

    /// Registered identifiers, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.plugins.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }
}

/// Transform plugins, keyed by GTS identifier.
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Register the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        let plugin: Arc<dyn TransformPlugin> = Arc::new(RequestIdTransformPlugin);
        plugins.insert(plugin.plugin_type().to_owned(), plugin);
        Self { plugins }
    }

    /// Add an externally-provided plugin.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, plugin_type: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_type).map(Arc::clone)
    }

    /// Whether `plugin_type` resolves.
    #[must_use]
    pub fn contains(&self, plugin_type: &str) -> bool {
        self.plugins.contains_key(plugin_type)
    }

    /// Registered identifiers, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self.plugins.keys().map(String::as_str).collect();
        ids.sort_unstable();
        ids
    }
}

/// The three registries, wired once and shared by the Data Plane.
pub struct PluginRegistries {
    /// Auth plugins.
    pub auth: AuthPluginRegistry,
    /// Guard plugins.
    pub guard: GuardPluginRegistry,
    /// Transform plugins.
    pub transform: TransformPluginRegistry,
}

impl crate::domain::plugin::PluginCatalog for PluginRegistries {
    fn has_auth(&self, plugin_type: &str) -> bool {
        self.auth.contains(plugin_type)
    }

    fn has_guard(&self, plugin_type: &str) -> bool {
        self.guard.contains(plugin_type)
    }

    fn has_transform(&self, plugin_type: &str) -> bool {
        self.transform.contains(plugin_type)
    }

    fn auth_ids(&self) -> Vec<String> {
        self.auth.ids().into_iter().map(str::to_owned).collect()
    }
}

impl PluginRegistries {
    /// Wire all built-ins.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_http_config: Option<toolkit_http::HttpClientConfig>,
        token_cache_config: TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(
                credstore,
                token_http_config,
                token_cache_config,
            ),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::{
        APIKEY_AUTH_PLUGIN_ID, BASIC_AUTH_PLUGIN_ID, BEARER_AUTH_PLUGIN_ID, CORS_GUARD_PLUGIN_ID,
        LOGGING_TRANSFORM_PLUGIN_ID, METRICS_TRANSFORM_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID,
        OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        REQUEST_ID_TRANSFORM_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID,
    };
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::time::Duration;

    fn registries() -> PluginRegistries {
        PluginRegistries::with_builtins(
            Arc::new(MockCredStoreClient::empty()),
            None,
            TokenCacheConfig {
                ttl: Duration::from_secs(300),
                capacity: 16,
            },
        )
    }

    #[test]
    fn auth_registry_holds_exactly_the_four_implemented_plugins() {
        let registries = registries();
        assert_eq!(registries.auth.ids(), {
            let mut expected = vec![
                APIKEY_AUTH_PLUGIN_ID,
                NOOP_AUTH_PLUGIN_ID,
                OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
                OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
            ];
            expected.sort_unstable();
            expected
        });
    }

    #[test]
    fn catalog_only_auth_ids_do_not_resolve() {
        let registries = registries();
        assert!(!registries.auth.contains(BASIC_AUTH_PLUGIN_ID));
        assert!(!registries.auth.contains(BEARER_AUTH_PLUGIN_ID));
        assert!(registries.auth.get(BASIC_AUTH_PLUGIN_ID).is_none());
    }

    #[test]
    fn required_headers_is_the_only_bindable_guard() {
        let registries = registries();
        assert_eq!(
            registries.guard.ids(),
            vec![REQUIRED_HEADERS_GUARD_PLUGIN_ID]
        );
        assert!(!registries.guard.contains(TIMEOUT_GUARD_PLUGIN_ID));
        assert!(!registries.guard.contains(CORS_GUARD_PLUGIN_ID));
    }

    #[test]
    fn request_id_is_the_only_resolvable_transform() {
        let registries = registries();
        assert_eq!(
            registries.transform.ids(),
            vec![REQUEST_ID_TRANSFORM_PLUGIN_ID]
        );
        assert!(!registries.transform.contains(LOGGING_TRANSFORM_PLUGIN_ID));
        assert!(!registries.transform.contains(METRICS_TRANSFORM_PLUGIN_ID));
    }

    #[test]
    fn external_plugins_can_be_registered() {
        let mut registry = AuthPluginRegistry::empty();
        assert!(registry.ids().is_empty());
        registry.register(Arc::new(NoopAuthPlugin));
        assert!(registry.contains(NOOP_AUTH_PLUGIN_ID));
    }
}
