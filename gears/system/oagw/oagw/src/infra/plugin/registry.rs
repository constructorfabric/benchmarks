//! Plugin registries, keyed by canonical GTS identifier.

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

/// Registry of [`AuthPlugin`] implementations.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry pre-populated with the built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn credstore_sdk::CredStoreClientV1>,
        token_cache_config: crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig,
    ) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(crate::infra::plugin::noop_auth::NoopAuthPlugin));
        registry.register(Arc::new(
            crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin::new(credstore.clone()),
        ));
        let form = crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
            credstore.clone(),
            toolkit_auth::oauth2::types::ClientAuthMethod::Form,
            token_cache_config.ttl,
            token_cache_config.capacity,
        );
        let basic = crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
            credstore,
            toolkit_auth::oauth2::types::ClientAuthMethod::Basic,
            token_cache_config.ttl,
            token_cache_config.capacity,
        );
        registry.register(Arc::new(form));
        registry.register(Arc::new(basic));
        registry
    }

    /// Register a plugin under its GTS identifier.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.gts_id(), plugin);
    }

    /// Resolve a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, gts_id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(gts_id).cloned()
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Registry of [`GuardPlugin`] implementations.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry pre-populated with the required-headers guard.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(
            crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin,
        ));
        registry
    }

    /// Register a plugin under its GTS identifier.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.gts_id(), plugin);
    }

    /// Resolve a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, gts_id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(gts_id).cloned()
    }

    /// Every GTS identifier the registry can resolve.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

/// Registry of [`TransformPlugin`] implementations.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry pre-populated with the request-id transform.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(
            crate::infra::plugin::request_id_transform::RequestIdTransformPlugin,
        ));
        registry
    }

    /// Register a plugin under its GTS identifier.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.gts_id(), plugin);
    }

    /// Resolve a plugin by GTS identifier.
    #[must_use]
    pub fn get(&self, gts_id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(gts_id).cloned()
    }

    /// Every GTS identifier the registry can resolve.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::plugin::gts_helpers;
    use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;

    #[test]
    fn built_in_auth_plugins_resolve_by_gts_id() {
        let registry = AuthPluginRegistry::with_builtins(
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
            TokenCacheConfig::default(),
        );
        for id in [
            gts_helpers::NOOP_AUTH,
            gts_helpers::APIKEY_AUTH,
            gts_helpers::OAUTH2_CLIENT_CRED,
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC,
        ] {
            assert!(registry.get(id).is_some(), "{id} must resolve");
        }
        assert!(registry.get(gts_helpers::CATALOG_ONLY_AUTH[0]).is_none());
    }

    #[test]
    fn built_in_guard_plugins_resolve_by_gts_id() {
        let registry = GuardPluginRegistry::with_builtins();
        assert!(registry.get(gts_helpers::REQUIRED_HEADERS_GUARD).is_some());
        assert!(registry.get(gts_helpers::CATALOG_ONLY_GUARD[0]).is_none());
        assert_eq!(registry.ids().len(), 1);
    }

    #[test]
    fn built_in_transform_plugins_resolve_by_gts_id() {
        let registry = TransformPluginRegistry::with_builtins();
        assert!(registry.get(gts_helpers::REQUEST_ID_TRANSFORM).is_some());
        assert!(
            registry
                .get(gts_helpers::CATALOG_ONLY_TRANSFORM[0])
                .is_none()
        );
    }

    #[test]
    fn catalog_only_identifiers_are_recognized() {
        assert!(super::super::is_catalog_only(
            gts_helpers::CATALOG_ONLY_TRANSFORM[0]
        ));
        assert!(!super::super::is_catalog_only(
            gts_helpers::REQUEST_ID_TRANSFORM
        ));
    }
}
