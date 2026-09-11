//! Plugin registries: built-ins, custom plugins and the unusable catalogue.

use super::request_id::RequestIdTransformPlugin;
use super::required_headers_guard::RequiredHeadersGuardPlugin;
use super::{
    AuthPlugin, GuardPlugin, TransformPlugin,
    apikey::ApiKeyAuthPlugin,
    noop::NoopAuthPlugin,
    oauth2_client_cred::{ClientAuthMethod, OAuth2ClientCredAuthPlugin, TokenFetcher},
};
use crate::domain::error::OagwError;
use crate::domain::model::AuthConfig;
use crate::gts_helpers;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Registry of credential-injection plugins.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry containing every built-in auth plugin.
    // The minute-based token TTL reads better as `from_mins`, which is unstable.
    #[allow(clippy::duration_suboptimal_units)]
    #[must_use]
    pub fn with_builtins(token_fetcher: &Arc<dyn TokenFetcher>) -> Self {
        let mut registry = Self::new();
        registry.insert(Arc::new(NoopAuthPlugin));
        registry.insert(Arc::new(ApiKeyAuthPlugin));
        registry.insert(Arc::new(OAuth2ClientCredAuthPlugin::new(
            Arc::clone(token_fetcher),
            ClientAuthMethod::Form,
            Duration::from_secs(5 * 60),
            super::oauth2_client_cred::default_cache_capacity(),
        )));
        registry.insert(Arc::new(OAuth2ClientCredAuthPlugin::new(
            Arc::clone(token_fetcher),
            ClientAuthMethod::Basic,
            Duration::from_secs(5 * 60),
            super::oauth2_client_cred::default_cache_capacity(),
        )));
        registry
    }

    /// Registers a plugin implementation.
    pub fn insert(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a plugin by identifier.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the identifier has no
    /// implementation, which includes the catalogue-only ids.
    pub fn get(&self, id: &str) -> Result<Arc<dyn AuthPlugin>, OagwError> {
        if let Some(plugin) = self.plugins.get(id) {
            return Ok(Arc::clone(plugin));
        }
        Err(unusable(id))
    }
}

/// Registry of guard plugins.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry containing every built-in guard plugin.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.insert(Arc::new(RequiredHeadersGuardPlugin));
        registry
    }

    /// Registers a plugin implementation.
    pub fn insert(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a plugin by identifier.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the identifier has no
    /// implementation.
    pub fn get(&self, id: &str) -> Result<Arc<dyn GuardPlugin>, OagwError> {
        self.plugins
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| unusable(id))
    }
}

/// Registry of transform plugins.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A registry containing every built-in transform plugin.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.insert(Arc::new(RequestIdTransformPlugin));
        registry
    }

    /// Registers a plugin implementation.
    pub fn insert(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a plugin by identifier.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::PluginNotFound`] when the identifier has no
    /// implementation.
    pub fn get(&self, id: &str) -> Result<Arc<dyn TransformPlugin>, OagwError> {
        self.plugins
            .get(id)
            .map(Arc::clone)
            .ok_or_else(|| unusable(id))
    }
}

/// The error raised for an identifier with no implementation.
fn unusable(id: &str) -> OagwError {
    OagwError::PluginNotFound(format!("plugin '{id}' has no implementation"))
}

/// Whether an auth configuration names a catalogue-only plugin.
#[must_use]
pub fn is_catalogue_only(id: &str) -> bool {
    matches!(id, gts_helpers::AUTH_BASIC | gts_helpers::AUTH_BEARER)
        || matches!(id, gts_helpers::GUARD_TIMEOUT | gts_helpers::GUARD_CORS)
        || matches!(
            id,
            gts_helpers::TRANSFORM_LOGGING | gts_helpers::TRANSFORM_METRICS
        )
}

/// The auth plugin identifier an upstream configuration selects.
#[must_use]
pub fn auth_plugin_id(config: &AuthConfig) -> String {
    config.plugin_type.clone()
}

/// Resolves a custom plugin identifier to its tenant-scoped instance.
#[must_use]
pub fn custom_plugin_id(id: &str, tenant: Uuid) -> String {
    format!("{id}:{tenant}")
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
