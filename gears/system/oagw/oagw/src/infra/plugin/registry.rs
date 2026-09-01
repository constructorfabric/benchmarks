// Created: 2026-08-29 by Constructor Tech
//! Runtime plugin registry (`with_builtins`).
//!
//! The registry owns the singleton built-in plugin instances. It is immutable
//! after construction: the only mutable state is the OAuth2 token cache held
//! by the OAuth2 plugins themselves.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::CredStoreClientV1;

use crate::domain::plugin::{AuthPlugin, GuardPlugin, PluginBinding, PluginError, TransformPlugin};
use crate::infra::plugin::auth::{ApiKeyAuth, NoopAuth, OAuth2ClientCredentials};
use crate::infra::plugin::guard::RequiredHeadersGuard;
use crate::infra::plugin::transform::RequestIdTransform;

/// Runtime plugin registry.
pub struct PluginRegistry {
    auth: HashMap<String, Arc<dyn AuthPlugin>>,
    guards: HashMap<String, Arc<dyn GuardPlugin>>,
    transforms: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("auth", &self.auth.keys().collect::<Vec<_>>())
            .field("guards", &self.guards.keys().collect::<Vec<_>>())
            .field("transforms", &self.transforms.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::empty()
    }
}

impl PluginRegistry {
    /// Registry without any built-in.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            auth: HashMap::new(),
            guards: HashMap::new(),
            transforms: HashMap::new(),
        }
    }

    /// Registry holding every built-in plugin.
    ///
    /// `credstore` is shared by the credential-resolving auth plugins; a
    /// missing store makes those plugins fail closed with `401` at proxy time.
    #[must_use]
    pub fn with_builtins(
        credstore: Option<Arc<dyn CredStoreClientV1>>,
        token_cache_ttl: Duration,
        token_cache_capacity: usize,
    ) -> Self {
        let mut registry = Self::empty();
        let noop: Arc<dyn AuthPlugin> = Arc::new(NoopAuth);
        let apikey: Arc<dyn AuthPlugin> = Arc::new(ApiKeyAuth::new(credstore.clone()));
        let oauth_form: Arc<dyn AuthPlugin> = Arc::new(OAuth2ClientCredentials::new(
            credstore.clone(),
            token_cache_capacity,
            token_cache_ttl,
            false,
        ));
        let oauth_basic: Arc<dyn AuthPlugin> = Arc::new(OAuth2ClientCredentials::new(
            credstore,
            token_cache_capacity,
            token_cache_ttl,
            true,
        ));
        registry.auth.insert(noop.id().to_owned(), noop);
        registry.auth.insert(apikey.id().to_owned(), apikey);
        registry.auth.insert(oauth_form.id().to_owned(), oauth_form);
        registry
            .auth
            .insert(oauth_basic.id().to_owned(), oauth_basic);

        let required: Arc<dyn GuardPlugin> = Arc::new(RequiredHeadersGuard);
        registry.guards.insert(required.id().to_owned(), required);

        let request_id: Arc<dyn TransformPlugin> = Arc::new(RequestIdTransform);
        registry
            .transforms
            .insert(request_id.id().to_owned(), request_id);
        registry
    }

    /// Register an additional auth plugin (used by tests and future extensions).
    pub fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.auth.insert(plugin.id().to_owned(), plugin);
    }

    /// Register an additional guard plugin.
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.guards.insert(plugin.id().to_owned(), plugin);
    }

    /// Register an additional transform plugin.
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.transforms.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolve an auth plugin implementation, or `None` when the reference has
    /// no implementation (catalog-only ids).
    #[must_use]
    pub fn auth_plugin(&self, binding: &PluginBinding) -> Option<Arc<dyn AuthPlugin>> {
        self.auth
            .get(crate::domain::model::plugin_instance(&binding.plugin_ref))
            .cloned()
    }

    /// Resolve a guard plugin implementation.
    #[must_use]
    pub fn guard_plugin(&self, binding: &PluginBinding) -> Option<Arc<dyn GuardPlugin>> {
        self.guards
            .get(crate::domain::model::plugin_instance(&binding.plugin_ref))
            .cloned()
    }

    /// Resolve a transform plugin implementation.
    #[must_use]
    pub fn transform_plugin(&self, binding: &PluginBinding) -> Option<Arc<dyn TransformPlugin>> {
        self.transforms
            .get(crate::domain::model::plugin_instance(&binding.plugin_ref))
            .cloned()
    }

    /// `true` when no implementation exists for `reference`.
    #[must_use]
    pub fn missing(&self, reference: &str) -> bool {
        let instance = crate::domain::model::plugin_instance(reference);
        !self.auth.contains_key(instance)
            && !self.guards.contains_key(instance)
            && !self.transforms.contains_key(instance)
    }
}

/// Error surfaced when a bound plugin has no implementation.
#[must_use]
pub fn plugin_not_found(reference: &str) -> PluginError {
    PluginError::Internal(format!("no implementation for plugin '{reference}'"))
}
