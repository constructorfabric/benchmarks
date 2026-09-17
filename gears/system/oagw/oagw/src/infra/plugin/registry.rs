//! In-process plugin registries (DESIGN §3.2 "Plugin Registry").
//!
//! - [`AuthPluginRegistry`] — exactly one auth plugin per upstream.
//! - [`GuardPluginRegistry`] — zero-or-more request/response guards.
//! - [`TransformPluginRegistry`] — zero-or-more request/response transforms.
//!
//! Built-ins are registered via the `*::with_builtins` constructors; callers
//! may additionally register custom in-process plugins. Registry lookups are
//! by full GTS identifier.

use std::collections::HashMap;
use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use parking_lot::RwLock;

use crate::config::TokenCacheConfig;
use crate::domain::gts as g;
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};
use crate::domain::plugin::SecretResolver;

use super::builtins::{
    ApiKeyAuthPlugin, ClientAuthMethod, NoopAuthPlugin, OAuth2ClientCredAuthPlugin,
    RequestIdTransformPlugin, RequiredHeadersGuardPlugin,
};
use super::CredStoreSecretResolver;
use crate::infra::proxy::client::OagwHttpClient;

/// Registry of named auth plugins (one per upstream).
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: RwLock<HashMap<String, Arc<dyn AuthPlugin>>>,
}

impl AuthPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Register a plugin. Duplicate identifiers are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when `plugin.id()` is already registered.
    pub fn register(&self, plugin: Arc<dyn AuthPlugin>) -> Result<(), String> {
        let mut map = self.plugins.write();
        if map.contains_key(plugin.id()) {
            return Err(format!("auth plugin '{}' already registered", plugin.id()));
        }
        map.insert(plugin.id().to_owned(), plugin);
        Ok(())
    }

    /// Look up a plugin by full GTS identifier.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.read().get(id).cloned()
    }

    /// Build the registry populated with the four built-in auth plugins
    /// (ADR 0008): `noop`, `apikey`, `oauth2_client_cred`,
    /// `oauth2_client_cred_basic`.
    #[must_use]
    pub fn with_builtins(
        credstore: Arc<dyn CredStoreClientV1>,
        http: Arc<OagwHttpClient>,
        cache: TokenCacheConfig,
    ) -> Arc<Self> {
        let secrets: Arc<dyn SecretResolver> =
            Arc::new(CredStoreSecretResolver::new(credstore));
        Self::with_builtins_resolver(secrets, http, cache)
    }

    /// Same as [`Self::with_builtins`] but with an explicit resolver
    /// (used by tests that do not want a real credstore).
    #[must_use]
    pub fn with_builtins_resolver(
        secrets: Arc<dyn SecretResolver>,
        http: Arc<OagwHttpClient>,
        cache: TokenCacheConfig,
    ) -> Arc<Self> {
        let registry = Arc::new(Self::empty());
        registry
            .register(Arc::new(NoopAuthPlugin))
            .expect("noop not registered");
        registry
            .register(Arc::new(ApiKeyAuthPlugin))
            .expect("apikey not registered");
        registry
            .register(Arc::new(OAuth2ClientCredAuthPlugin::new(
                g::AUTH_OAUTH2_CLIENT_CRED,
                ClientAuthMethod::Form,
                secrets.clone(),
                http.clone(),
                cache,
            )))
            .expect("oauth2_client_cred not registered");
        registry
            .register(Arc::new(OAuth2ClientCredAuthPlugin::new(
                g::AUTH_OAUTH2_CLIENT_CRED_BASIC,
                ClientAuthMethod::Basic,
                secrets,
                http,
                cache,
            )))
            .expect("oauth2_client_cred_basic not registered");
        registry
    }
}

/// Registry of named guard plugins.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: RwLock<HashMap<String, Arc<dyn GuardPlugin>>>,
}

impl GuardPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Register a plugin. Duplicate identifiers are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when `plugin.id()` is already registered.
    pub fn register(&self, plugin: Arc<dyn GuardPlugin>) -> Result<(), String> {
        let mut map = self.plugins.write();
        if map.contains_key(plugin.id()) {
            return Err(format!("guard plugin '{}' already registered", plugin.id()));
        }
        map.insert(plugin.id().to_owned(), plugin);
        Ok(())
    }

    /// Look up a plugin by full GTS identifier.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.read().get(id).cloned()
    }

    /// Build the registry with the built-in `required_headers` guard
    /// (ADR 0009). `timeout`/`cors` are core data-plane logic, not guards,
    /// and remain catalog-only.
    #[must_use]
    pub fn with_builtins() -> Arc<Self> {
        let registry = Arc::new(Self::empty());
        registry
            .register(Arc::new(RequiredHeadersGuardPlugin))
            .expect("required_headers not registered");
        registry
    }
}

/// Registry of named transform plugins.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: RwLock<HashMap<String, Arc<dyn TransformPlugin>>>,
}

impl TransformPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Register a plugin. Duplicate identifiers are rejected.
    ///
    /// # Errors
    ///
    /// Returns an error when `plugin.id()` is already registered.
    pub fn register(&self, plugin: Arc<dyn TransformPlugin>) -> Result<(), String> {
        let mut map = self.plugins.write();
        if map.contains_key(plugin.id()) {
            return Err(format!(
                "transform plugin '{}' already registered",
                plugin.id()
            ));
        }
        map.insert(plugin.id().to_owned(), plugin);
        Ok(())
    }

    /// Look up a plugin by full GTS identifier.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.read().get(id).cloned()
    }

    /// Build the registry with the built-in `request_id` transform.
    /// `logging`/`metrics` are core data-plane instrumentation, not
    /// transforms, and remain catalog-only.
    #[must_use]
    pub fn with_builtins() -> Arc<Self> {
        let registry = Arc::new(Self::empty());
        registry
            .register(Arc::new(RequestIdTransformPlugin))
            .expect("request_id not registered");
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use uuid::Uuid;

    struct StaticSecretResolver(HashMap<String, String>);
    #[async_trait]
    impl SecretResolver for StaticSecretResolver {
        async fn resolve(
            &self,
            _tenant_id: Uuid,
            secret_ref: &str,
        ) -> Result<Vec<u8>, crate::domain::error::DomainError> {
            self.0
                .get(secret_ref)
                .map(|v| v.clone().into_bytes())
                .ok_or_else(|| crate::domain::error::DomainError::SecretNotFound(secret_ref.into()))
        }
    }

    fn builtin_auth_registry() -> Arc<AuthPluginRegistry> {
        let http = Arc::new(OagwHttpClient::new());
        AuthPluginRegistry::with_builtins_resolver(
            Arc::new(StaticSecretResolver(HashMap::new())),
            http,
            TokenCacheConfig::default(),
        )
    }

    #[test]
    fn auth_builtins_are_registered() {
        let registry = builtin_auth_registry();
        assert!(registry.get(g::AUTH_NOOP).is_some());
        assert!(registry.get(g::AUTH_APIKEY).is_some());
        assert!(registry.get(g::AUTH_OAUTH2_CLIENT_CRED).is_some());
        assert!(registry.get(g::AUTH_OAUTH2_CLIENT_CRED_BASIC).is_some());
    }

    #[test]
    fn catalog_only_auth_plugins_are_not_resolvable() {
        let registry = builtin_auth_registry();
        assert!(registry.get(g::AUTH_BASIC).is_none());
        assert!(registry.get(g::AUTH_BEARER).is_none());
    }

    #[test]
    fn guard_registry_exposes_only_required_headers() {
        let registry = GuardPluginRegistry::with_builtins();
        assert!(registry.get(g::GUARD_REQUIRED_HEADERS).is_some());
        // timeout/cors are core data-plane functionality, not guards.
        assert!(registry.get(g::GUARD_TIMEOUT).is_none());
        assert!(registry.get(g::GUARD_CORS).is_none());
    }

    #[test]
    fn transform_registry_exposes_only_request_id() {
        let registry = TransformPluginRegistry::with_builtins();
        assert!(registry.get(g::TRANSFORM_REQUEST_ID).is_some());
        assert!(registry.get(g::TRANSFORM_LOGGING).is_none());
        assert!(registry.get(g::TRANSFORM_METRICS).is_none());
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let registry = GuardPluginRegistry::empty();
        assert!(registry
            .register(Arc::new(RequiredHeadersGuardPlugin))
            .is_ok());
        assert!(registry
            .register(Arc::new(RequiredHeadersGuardPlugin))
            .is_err());
    }

    #[test]
    fn empty_reports_missing() {
        let registry = AuthPluginRegistry::empty();
        assert!(registry.get(g::AUTH_NOOP).is_none());
    }
}
