//! Plugin registries (ADR-0002 "Plugin Loading").
//!
//! Named plugins live in-process and are addressed by the instance part of
//! their GTS identifier; UUID-backed references point at a stored `oagw_plugin`
//! row and resolve to the custom plugin, which has no executable
//! implementation in this build.

use std::collections::HashMap;
use std::sync::Arc;

use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::error::DomainError;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::ids;

use super::apikey_auth::ApiKeyAuthPlugin;
use super::noop_auth::NoopAuthPlugin;
use super::oauth2_cc_auth::OAuth2ClientCredAuthPlugin;
use super::request_id_transform::RequestIdTransformPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;
use super::token_cache::TokenCacheConfig;

/// Registry of auth plugins.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

/// Registry of guard plugins.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

/// Registry of transform plugins.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

/// All three registries, bundled so the data plane carries one handle.
#[derive(Clone, Default)]
pub struct PluginRegistries {
    /// Credential-injection plugins.
    pub auth: AuthPluginRegistry,
    /// Policy-enforcement plugins.
    pub guard: GuardPluginRegistry,
    /// Mutation plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Registries holding every built-in plugin (ADR-0002, ADR-0008, ADR-0009).
    #[must_use]
    pub fn with_builtins(
        resolver: &crate::infra::credentials::SecretResolver,
        cache: &TokenCacheConfig,
    ) -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(resolver, cache),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }
}

impl AuthPluginRegistry {
    /// Register the four built-in auth plugins.
    #[must_use]
    pub fn with_builtins(
        resolver: &crate::infra::credentials::SecretResolver,
        cache: &TokenCacheConfig,
    ) -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(NoopAuthPlugin));
        registry.register(Arc::new(ApiKeyAuthPlugin::new(resolver.clone())));
        registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            ClientAuthMethod::Form,
            *cache,
        )));
        registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
            resolver.clone(),
            ClientAuthMethod::Basic,
            *cache,
        )));
        registry
    }

    /// Add a plugin under its registry id and its GTS instance id.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin.clone());
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a plugin reference.
    ///
    /// # Errors
    ///
    /// Returns `cf.oagw.plugin.not_found.v1` for catalog-only identifiers and
    /// for names that are not registered.
    pub fn resolve(&self, plugin_ref: &str) -> Result<Arc<dyn AuthPlugin>, DomainError> {
        self.plugins.get(&key(plugin_ref)).cloned().ok_or_else(|| {
            DomainError::plugin_not_found(format!("auth plugin `{plugin_ref}` is not resolvable"))
        })
    }

    /// Number of registered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry holds no plugins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

impl GuardPluginRegistry {
    /// Register the built-in guard plugin (ADR-0009).
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(RequiredHeadersGuardPlugin));
        registry
    }

    /// Add a plugin under its registry id and its GTS instance id.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin.clone());
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a plugin reference.
    ///
    /// # Errors
    ///
    /// Returns `cf.oagw.plugin.not_found.v1` for catalog-only identifiers and
    /// for names that are not registered.
    pub fn resolve(&self, plugin_ref: &str) -> Result<Arc<dyn GuardPlugin>, DomainError> {
        self.plugins.get(&key(plugin_ref)).cloned().ok_or_else(|| {
            DomainError::plugin_not_found(format!("guard plugin `{plugin_ref}` is not resolvable"))
        })
    }
}

impl TransformPluginRegistry {
    /// Register the built-in transform plugin.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// Add a plugin under its registry id and its GTS instance id.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin.clone());
        self.plugins.insert(plugin.plugin_type().to_owned(), plugin);
    }

    /// Resolve a plugin reference.
    ///
    /// # Errors
    ///
    /// Returns `cf.oagw.plugin.not_found.v1` for catalog-only identifiers and
    /// for names that are not registered.
    pub fn resolve(&self, plugin_ref: &str) -> Result<Arc<dyn TransformPlugin>, DomainError> {
        self.plugins.get(&key(plugin_ref)).cloned().ok_or_else(|| {
            DomainError::plugin_not_found(format!(
                "transform plugin `{plugin_ref}` is not resolvable"
            ))
        })
    }
}

/// Registry lookup key for a wire plugin reference.
fn key(plugin_ref: &str) -> String {
    ids::instance_part(plugin_ref).to_owned()
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod registry_tests;
