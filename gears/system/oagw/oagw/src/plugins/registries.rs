//! Plugin registries.
//!
//! Built-ins are registered in `*_registry::with_builtins`; external gears
//! contribute additional implementations by registering against the same
//! registry.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;

use super::{
    ApiKeyAuthPlugin, AuthPlugin, GuardPlugin, NoopAuthPlugin, OAuth2ClientCredAuthPlugin,
    RequestIdTransformPlugin, RequiredHeadersGuardPlugin, TransformPlugin,
};

/// Registry of [`AuthPlugin`] implementations.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: Vec<Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry seeded with the built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        token_cache_ttl: std::time::Duration,
        token_cache_capacity: usize,
    ) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(NoopAuthPlugin));
        registry.register(Arc::new(ApiKeyAuthPlugin::new(credstore.clone())));
        registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
            credstore.clone(),
            crate::plugins::oauth2_client_cred::ClientAuthMethodTag::Form,
            token_cache_ttl,
            token_cache_capacity,
        )));
        registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
            credstore,
            crate::plugins::oauth2_client_cred::ClientAuthMethodTag::Basic,
            token_cache_ttl,
            token_cache_capacity,
        )));
        registry
    }

    /// Registers an implementation.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.push(plugin);
    }

    /// Resolves a plugin by GTS identifier.
    #[must_use]
    pub fn resolve(&self, plugin_type: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins
            .iter()
            .find(|p| p.plugin_type() == plugin_type)
            .cloned()
    }

    /// GTS identifiers of the registered plugins.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins
            .iter()
            .map(|p| p.plugin_type().to_owned())
            .collect()
    }
}

/// Registry of [`GuardPlugin`] implementations.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: Vec<Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry seeded with the built-in guard plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(RequiredHeadersGuardPlugin));
        registry
    }

    /// Registers an implementation.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.push(plugin);
    }

    /// Resolves a plugin by GTS identifier.
    #[must_use]
    pub fn resolve(&self, plugin_type: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins
            .iter()
            .find(|p| p.plugin_type() == plugin_type)
            .cloned()
    }
}

/// Registry of [`TransformPlugin`] implementations.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: Vec<Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry seeded with the built-in transform plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// Registers an implementation.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.push(plugin);
    }

    /// Resolves a plugin by GTS identifier.
    #[must_use]
    pub fn resolve(&self, plugin_type: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins
            .iter()
            .find(|p| p.plugin_type() == plugin_type)
            .cloned()
    }
}
