//! Plugin registries (ADR 0008, ADR 0009) — the in-process resolution point for
//! named (built-in) plugin identifiers.

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::gts_helpers as gts;
use crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin;
use crate::infra::plugin::noop_auth::NoopAuthPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin;
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;
use toolkit_auth::oauth2::ClientAuthMethod;

/// Errors raised when a plugin reference cannot be resolved.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginResolveError {
    /// The identifier is a catalog-only one with no backing implementation.
    #[error("plugin `{0}` is a catalog identifier with no backing implementation")]
    CatalogOnly(String),
    /// The identifier is neither built-in nor a stored custom plugin.
    #[error("unknown plugin `{0}`")]
    Unknown(String),
}

/// Registry of [`AuthPlugin`] implementations, keyed by GTS identifier.
#[derive(Clone, Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn crate::domain::plugin::AuthPlugin>>,
}

impl AuthPluginRegistry {
    /// Registry containing every built-in auth plugin (ADR 0008).
    #[must_use]
    pub fn with_builtins(
        credstore: &Arc<dyn credstore_sdk::CredStoreClientV1>,
        config: &crate::config::OagwConfig,
    ) -> Self {
        let mut plugins: HashMap<String, Arc<dyn crate::domain::plugin::AuthPlugin>> =
            HashMap::new();
        let ttl = config.token_cache.ttl();
        let capacity = config.token_cache.capacity;
        for (id, plugin) in [
            (
                gts::AUTH_NOOP,
                Arc::new(NoopAuthPlugin) as Arc<dyn crate::domain::plugin::AuthPlugin>,
            ),
            (
                gts::AUTH_APIKEY,
                Arc::new(ApiKeyAuthPlugin::new(credstore.clone())),
            ),
            (
                gts::AUTH_OAUTH2_CC,
                Arc::new(OAuth2ClientCredAuthPlugin::new(
                    credstore.clone(),
                    ClientAuthMethod::Form,
                    ttl,
                    capacity,
                )),
            ),
            (
                gts::AUTH_OAUTH2_CC_BASIC,
                Arc::new(OAuth2ClientCredAuthPlugin::new(
                    credstore.clone(),
                    ClientAuthMethod::Basic,
                    ttl,
                    capacity,
                )),
            ),
        ] {
            plugins.insert(id.to_owned(), plugin);
        }
        Self { plugins }
    }

    /// An empty registry; the data plane adds the built-ins it was given.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Registers an additional plugin implementation.
    pub fn register(&mut self, id: &str, plugin: Arc<dyn crate::domain::plugin::AuthPlugin>) {
        self.plugins.insert(id.to_owned(), plugin);
    }

    /// Resolves a plugin reference against the built-in registry.
    ///
    /// # Errors
    /// Returns [`PluginResolveError`] when the reference is unknown.
    pub fn get(
        &self,
        id: &str,
    ) -> Result<Arc<dyn crate::domain::plugin::AuthPlugin>, PluginResolveError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| catalog_or_unknown(id))
    }

    /// Every registered identifier.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Resolves a reference, returning `None` for custom-plugin UUIDs: those are
    /// catalogued by the Control Plane but are not executable built-ins.
    ///
    /// # Errors
    /// Returns [`PluginResolveError`] for catalog-only or unknown identifiers.
    pub fn resolve(
        &self,
        id: &str,
    ) -> Result<Option<Arc<dyn crate::domain::plugin::AuthPlugin>>, PluginResolveError> {
        if let Some(plugin) = self.plugins.get(id) {
            return Ok(Some(plugin.clone()));
        }
        if is_custom_ref(id) {
            return Ok(None);
        }
        Err(catalog_or_unknown(id))
    }
}

/// Registry of [`GuardPlugin`] implementations.
#[derive(Clone, Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn crate::domain::plugin::GuardPlugin>>,
}

impl GuardPluginRegistry {
    /// Registry containing the built-in guards. `timeout` and `cors` are catalog
    /// identifiers only — CORS and timeout enforcement are core data-plane logic.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn crate::domain::plugin::GuardPlugin>> =
            HashMap::new();
        plugins.insert(
            gts::GUARD_REQUIRED_HEADERS.to_owned(),
            Arc::new(RequiredHeadersGuardPlugin) as Arc<dyn crate::domain::plugin::GuardPlugin>,
        );
        Self { plugins }
    }

    /// An empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Registers an additional plugin implementation.
    pub fn register(&mut self, id: &str, plugin: Arc<dyn crate::domain::plugin::GuardPlugin>) {
        self.plugins.insert(id.to_owned(), plugin);
    }

    /// Resolves a plugin reference.
    ///
    /// # Errors
    /// Returns [`PluginResolveError`] when the reference is unknown.
    pub fn get(
        &self,
        id: &str,
    ) -> Result<Arc<dyn crate::domain::plugin::GuardPlugin>, PluginResolveError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| catalog_or_unknown(id))
    }

    /// Resolves a reference, returning `None` for custom-plugin UUIDs.
    ///
    /// # Errors
    /// Returns [`PluginResolveError`] for catalog-only or unknown identifiers.
    pub fn resolve(
        &self,
        id: &str,
    ) -> Result<Option<Arc<dyn crate::domain::plugin::GuardPlugin>>, PluginResolveError> {
        if let Some(plugin) = self.plugins.get(id) {
            return Ok(Some(plugin.clone()));
        }
        if is_custom_ref(id) {
            return Ok(None);
        }
        Err(catalog_or_unknown(id))
    }
}

/// Registry of [`TransformPlugin`] implementations.
#[derive(Clone, Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn crate::domain::plugin::TransformPlugin>>,
}

impl TransformPluginRegistry {
    /// Registry containing the built-in transforms. `logging` and `metrics` are
    /// catalog identifiers only.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn crate::domain::plugin::TransformPlugin>> =
            HashMap::new();
        plugins.insert(
            gts::TRANSFORM_REQUEST_ID.to_owned(),
            Arc::new(RequestIdTransformPlugin) as Arc<dyn crate::domain::plugin::TransformPlugin>,
        );
        Self { plugins }
    }

    /// Registers an additional plugin implementation.
    pub fn register(&mut self, id: &str, plugin: Arc<dyn crate::domain::plugin::TransformPlugin>) {
        self.plugins.insert(id.to_owned(), plugin);
    }

    /// An empty registry.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Resolves a plugin reference.
    ///
    /// # Errors
    /// Returns [`PluginResolveError`] when the reference is unknown.
    pub fn get(
        &self,
        id: &str,
    ) -> Result<Arc<dyn crate::domain::plugin::TransformPlugin>, PluginResolveError> {
        self.plugins
            .get(id)
            .cloned()
            .ok_or_else(|| catalog_or_unknown(id))
    }

    /// Resolves a reference, returning `None` for custom-plugin UUIDs.
    ///
    /// # Errors
    /// Returns [`PluginResolveError`] for catalog-only or unknown identifiers.
    pub fn resolve(
        &self,
        id: &str,
    ) -> Result<Option<Arc<dyn crate::domain::plugin::TransformPlugin>>, PluginResolveError> {
        if let Some(plugin) = self.plugins.get(id) {
            return Ok(Some(plugin.clone()));
        }
        if is_custom_ref(id) {
            return Ok(None);
        }
        Err(catalog_or_unknown(id))
    }
}

/// `true` when `id` is a bare UUID, i.e. a stored custom plugin reference.
fn is_custom_ref(id: &str) -> bool {
    crate::domain::gts_helpers::uuid_from_resource_id(id).is_some()
}

impl From<PluginResolveError> for crate::domain::error::DomainError {
    fn from(value: PluginResolveError) -> Self {
        Self::PluginNotFound(value.to_string())
    }
}

/// Distinguishes a documented-but-unresolvable catalog identifier from a truly
/// unknown one.
fn catalog_or_unknown(id: &str) -> PluginResolveError {
    if matches!(
        id,
        gts::AUTH_BASIC
            | gts::AUTH_BEARER
            | gts::GUARD_TIMEOUT
            | gts::GUARD_CORS
            | gts::TRANSFORM_LOGGING
            | gts::TRANSFORM_METRICS
    ) {
        PluginResolveError::CatalogOnly(id.to_owned())
    } else {
        PluginResolveError::Unknown(id.to_owned())
    }
}
