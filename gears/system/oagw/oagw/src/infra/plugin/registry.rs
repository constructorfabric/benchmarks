//! In-process plugin registries.
//!
//! Named plugins live here and are never persisted; catalog-only identifiers
//! are deliberately absent, so binding one fails at write time with
//! `unknown plugin` (ADR-0002, ADR-0009).

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::domain::ports::PluginCatalog;
use crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin;
use crate::infra::plugin::noop_auth::NoopAuthPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::{OAuth2ClientCredAuthPlugin, TokenCacheConfig};
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;

/// Auth plugin registry. One plugin per upstream.
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Register the built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache: TokenCacheConfig,
    ) -> Self {
        let mut plugins: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        let register = |plugins: &mut HashMap<String, Arc<dyn AuthPlugin>>,
                        plugin: Arc<dyn AuthPlugin>| {
            plugins.insert(plugin.plugin_type().to_owned(), plugin);
        };
        register(&mut plugins, Arc::new(NoopAuthPlugin));
        register(
            &mut plugins,
            Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&credstore))),
        );
        register(
            &mut plugins,
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                Arc::clone(&credstore),
                ClientAuthMethod::Form,
                token_cache,
            )),
        );
        register(
            &mut plugins,
            Arc::new(OAuth2ClientCredAuthPlugin::new(
                credstore,
                ClientAuthMethod::Basic,
                token_cache,
            )),
        );
        Self { plugins }
    }

    /// Add an externally-provided plugin (e.g. from another gear).
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
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

/// Guard plugin registry.
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Register the built-in guard plugins. `required_headers` is currently
    /// the only entry: timeout and CORS are core Data Plane logic.
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

    /// Resolve a named guard plugin.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_ref).map(Arc::clone)
    }
}

impl Default for GuardPluginRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// Transform plugin registry.
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Register the built-in transform plugins. `request_id` is the only
    /// entry: logging and metrics are core instrumentation.
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

    /// Resolve a named transform plugin.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_ref).map(Arc::clone)
    }
}

impl Default for TransformPluginRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

/// The three registries as one unit, and the [`PluginCatalog`] the Control
/// Plane consults at write time.
pub struct PluginRegistries {
    /// Auth plugins.
    pub auth: AuthPluginRegistry,
    /// Guard plugins.
    pub guard: GuardPluginRegistry,
    /// Transform plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Build all three with their built-in entries.
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

impl PluginCatalog for PluginRegistries {
    fn has_auth(&self, plugin_ref: &str) -> bool {
        self.auth.get(plugin_ref).is_some()
    }

    fn has_guard(&self, plugin_ref: &str) -> bool {
        self.guard.get(plugin_ref).is_some()
    }

    fn has_transform(&self, plugin_ref: &str) -> bool {
        self.transform.get(plugin_ref).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::PluginRegistries;
    use crate::domain::gts_helpers as gts;
    use crate::domain::ports::PluginCatalog;
    use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
    use credstore_sdk::test_util::MockCredStoreClient;
    use std::sync::Arc;

    fn registries() -> PluginRegistries {
        PluginRegistries::with_builtins(
            Arc::new(MockCredStoreClient::empty()),
            TokenCacheConfig::default(),
        )
    }

    #[test]
    fn every_documented_builtin_resolves() {
        let r = registries();
        for id in [
            gts::NOOP_AUTH_PLUGIN_ID,
            gts::APIKEY_AUTH_PLUGIN_ID,
            gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
            gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
        ] {
            assert!(r.has_auth(id), "{id} must resolve");
        }
        assert!(r.has_guard(gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID));
        assert!(r.has_transform(gts::REQUEST_ID_TRANSFORM_PLUGIN_ID));
    }

    #[test]
    fn catalog_only_identifiers_do_not_resolve() {
        let r = registries();
        assert!(!r.has_auth(gts::BASIC_AUTH_PLUGIN_ID));
        assert!(!r.has_auth(gts::BEARER_AUTH_PLUGIN_ID));
        assert!(!r.has_guard(gts::TIMEOUT_GUARD_PLUGIN_ID));
        assert!(!r.has_guard(gts::CORS_GUARD_PLUGIN_ID));
        assert!(!r.has_transform(gts::LOGGING_TRANSFORM_PLUGIN_ID));
        assert!(!r.has_transform(gts::METRICS_TRANSFORM_PLUGIN_ID));
    }

    #[test]
    fn the_two_oauth2_variants_are_registered_separately() {
        let r = registries();
        let form = r
            .auth
            .get(gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID)
            .expect("form");
        let basic = r
            .auth
            .get(gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID)
            .expect("basic");
        assert_ne!(form.id(), basic.id());
    }

    #[test]
    fn registries_do_not_cross_wire_plugin_kinds() {
        let r = registries();
        assert!(!r.has_guard(gts::APIKEY_AUTH_PLUGIN_ID));
        assert!(!r.has_transform(gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID));
        assert!(!r.has_auth(gts::REQUEST_ID_TRANSFORM_PLUGIN_ID));
    }
}
