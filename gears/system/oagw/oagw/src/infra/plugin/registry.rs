//! Plugin registries (ADR 0002 "Plugin Loading", ADR 0009 "Registry
//! Integration").
//!
//! A registry is a `HashMap<GTS instance id, Arc<dyn Plugin>>`. Built-ins are
//! installed by the `with_builtins()` constructors; external gears install
//! theirs with `register`. Lookups are by the canonical plugin identifier
//! string, so a bound `plugins.items[].plugin_ref` resolves directly.

use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, NOOP_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID, REQUEST_ID_TRANSFORM_PLUGIN_ID,
    REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

/// Registry of auth plugins.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: HashMap<String, Arc<dyn AuthPlugin>>,
}

impl std::fmt::Debug for AuthPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.plugins.keys()).finish()
    }
}

impl AuthPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in auth plugins.
    ///
    /// Every identifier that resolves here has a complete implementation. The
    /// credential-injection plugins are built over a runtime with **no**
    /// credstore, so they fail with a secret error rather than forwarding an
    /// unauthenticated request; gear deployments should prefer
    /// [`AuthPluginRegistry::with_builtins_for`], which wires the real
    /// credstore-backed runtime.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self::with_builtins_for(crate::infra::proxy::runtime::PluginRuntime::null())
    }

    /// The built-in auth plugins over a real [`PluginRuntime`].
    #[must_use]
    pub fn with_builtins_for(runtime: Arc<crate::infra::proxy::runtime::PluginRuntime>) -> Self {
        let mut plugins: HashMap<String, Arc<dyn AuthPlugin>> = HashMap::new();
        plugins.insert(
            NOOP_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(crate::infra::plugin::noop::NoopAuthPlugin),
        );
        plugins.insert(
            APIKEY_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin::new(
                Arc::clone(&runtime),
            )),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(
                crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::with_form(
                    Arc::clone(&runtime),
                ),
            ),
        );
        plugins.insert(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID.to_owned(),
            Arc::new(
                crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::with_basic(
                    runtime,
                ),
            ),
        );
        Self { plugins }
    }

    /// Register a plugin, keyed by its own id.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Look a plugin up by its identifier.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(plugin_ref).cloned()
    }

    /// Registered identifiers, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Number of registered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Registry of guard plugins.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: HashMap<String, Arc<dyn GuardPlugin>>,
}

impl std::fmt::Debug for GuardPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.plugins.keys()).finish()
    }
}

impl GuardPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in guard plugins.
    ///
    /// ADR 0009: `required_headers` is the only guard identifier resolvable
    /// via `GuardPluginRegistry`; `timeout` and `cors` are core Data Plane
    /// logic and are deliberately absent.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn GuardPlugin>> = HashMap::new();
        plugins.insert(
            REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            Arc::new(crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin),
        );
        Self { plugins }
    }

    /// Register a plugin, keyed by its own id.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Look a plugin up by its identifier.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(plugin_ref).cloned()
    }

    /// Registered identifiers, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Number of registered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Registry of transform plugins.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: HashMap<String, Arc<dyn TransformPlugin>>,
}

impl std::fmt::Debug for TransformPluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.plugins.keys()).finish()
    }
}

impl TransformPluginRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The built-in transform plugins.
    ///
    /// `request_id` is the only transform resolvable via this registry;
    /// `logging` and `metrics` are core Data Plane instrumentation.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut plugins: HashMap<String, Arc<dyn TransformPlugin>> = HashMap::new();
        plugins.insert(
            REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
            Arc::new(crate::infra::plugin::request_id::RequestIdTransformPlugin),
        );
        Self { plugins }
    }

    /// Register a plugin, keyed by its own id.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Look a plugin up by its identifier.
    #[must_use]
    pub fn get(&self, plugin_ref: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(plugin_ref).cloned()
    }

    /// Registered identifiers, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.plugins.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Number of registered plugins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// True when nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// The three registries the data plane consults when resolving a chain.
#[derive(Debug, Default)]
pub struct PluginRegistries {
    /// Auth plugins.
    pub auth: AuthPluginRegistry,
    /// Guard plugins.
    pub guard: GuardPluginRegistry,
    /// Transform plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Registries holding only the built-in plugins.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self {
            auth: AuthPluginRegistry::with_builtins(),
            guard: GuardPluginRegistry::with_builtins(),
            transform: TransformPluginRegistry::with_builtins(),
        }
    }

    /// True when a named (non-UUID) plugin reference resolves in the matching
    /// registry.
    #[must_use]
    pub fn resolves(
        &self,
        plugin_kind: crate::domain::model::PluginKind,
        plugin_ref: &str,
    ) -> bool {
        match plugin_kind {
            crate::domain::model::PluginKind::Auth => self.auth.get(plugin_ref).is_some(),
            crate::domain::model::PluginKind::Guard => self.guard.get(plugin_ref).is_some(),
            crate::domain::model::PluginKind::Transform => self.transform.get(plugin_ref).is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::{BASIC_AUTH_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID};

    #[test]
    fn builtins_are_registered_under_their_gts_ids() {
        let registries = PluginRegistries::with_builtins();
        assert!(registries.auth.get(NOOP_AUTH_PLUGIN_ID).is_some());
        assert!(registries.auth.get(APIKEY_AUTH_PLUGIN_ID).is_some());
        assert!(
            registries
                .auth
                .get(OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID)
                .is_some()
        );
        assert!(
            registries
                .auth
                .get(OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID)
                .is_some()
        );
        assert!(
            registries
                .guard
                .get(REQUIRED_HEADERS_GUARD_PLUGIN_ID)
                .is_some()
        );
        assert!(
            registries
                .transform
                .get(REQUEST_ID_TRANSFORM_PLUGIN_ID)
                .is_some()
        );
    }

    #[test]
    fn catalog_only_identifiers_are_not_resolvable() {
        let registries = PluginRegistries::with_builtins();
        assert!(registries.auth.get(BASIC_AUTH_PLUGIN_ID).is_none());
        assert!(registries.auth.get(TIMEOUT_GUARD_PLUGIN_ID).is_none());
    }
}
