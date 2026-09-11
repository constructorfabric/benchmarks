//! The plugin registry.
//!
//! Built-in plugins register themselves by their GTS identifier; a bound
//! plugin id the registry does not know is a `PluginNotFound` (503) at
//! request time rather than a silent skip.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::domain::plugin::{AuthPlugin, GuardPlugin, TransformPlugin};

/// Where a plugin reference resolves, per DESIGN §"Resolution Algorithm".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginResolution {
    /// A persisted, tenant-owned plugin (`{type}~{uuid}`), resolved through
    /// the plugin store.
    Persisted {
        /// The extracted instance part, when it parsed as a UUID.
        uuid: Option<uuid::Uuid>,
    },
    /// A named plugin, resolved through the in-process registry.
    Named {
        /// Whether this registry can execute it.
        resolvable: bool,
    },
}

/// Every plugin the gear can execute, indexed by kind.
#[derive(Default)]
pub struct PluginRegistry {
    auth: BTreeMap<String, Arc<dyn AuthPlugin>>,
    guards: BTreeMap<String, Arc<dyn GuardPlugin>>,
    transforms: BTreeMap<String, Arc<dyn TransformPlugin>>,
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

impl PluginRegistry {
    /// An empty registry.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The registry with the built-in and catalog plugins.
    pub fn builtin() -> Self {
        let mut registry = Self::empty();
        registry.register_auth(Arc::new(super::noop_auth::NoopAuth));
        registry.register_auth(Arc::new(super::api_key_auth::ApiKeyAuth::default()));
        registry.register_auth(Arc::new(super::oauth2_client_credentials::ClientCredentialsAuth::default()));
        registry.register_auth(Arc::new(super::oauth2_client_credentials::ClientCredentialsAuth::basic()));
        registry.register_guard(Arc::new(super::required_headers_guard::RequiredHeadersGuard));
        registry.register_transform(Arc::new(super::request_id_transform::RequestIdTransform));
        registry
    }

    /// Adds an auth plugin, replacing any with the same id.
    pub fn register_auth(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.auth.insert(plugin.id().to_string(), plugin);
    }

    /// Adds a guard plugin, replacing any with the same id.
    pub fn register_guard(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.guards.insert(plugin.id().to_string(), plugin);
    }

    /// Adds a transform plugin, replacing any with the same id.
    pub fn register_transform(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.transforms.insert(plugin.id().to_string(), plugin);
    }

    /// Looks up an auth plugin.
    pub fn auth(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.auth.get(id).cloned()
    }

    /// Looks up a guard plugin.
    pub fn guard(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.guards.get(id).cloned()
    }

    /// Looks up a transform plugin.
    pub fn transform(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.transforms.get(id).cloned()
    }

    /// The ids of the registered auth plugins.
    pub fn auth_ids(&self) -> Vec<String> {
        self.auth.keys().cloned().collect()
    }

    /// The ids of the registered guard plugins.
    pub fn guard_ids(&self) -> Vec<String> {
        self.guards.keys().cloned().collect()
    }

    /// The ids of the registered transform plugins.
    pub fn transform_ids(&self) -> Vec<String> {
        self.transforms.keys().cloned().collect()
    }

    /// Classifies a plugin reference per DESIGN §"Resolution Algorithm": a
    /// UUID instance addresses a persisted, tenant-owned plugin that the
    /// plugin store owns, and any other instance resolves here.
    pub fn classify(&self, id: &str) -> PluginResolution {
        match crate::domain::gts_helpers::plugin_uuid(id) {
            Some(uuid) => PluginResolution::Persisted { uuid: Some(uuid) },
            None => PluginResolution::Named {
                resolvable: self.is_registered(id),
            },
        }
    }

    /// Whether any kind of plugin is registered under `id`.
    fn is_registered(&self, id: &str) -> bool {
        self.auth.contains_key(id) || self.guards.contains_key(id) || self.transforms.contains_key(id)
    }

    /// Whether the registry can execute the named plugin `id` refers to. A
    /// persisted (UUID-backed) plugin is resolved through the plugin store
    /// instead, so it is never "resolved" here.
    pub fn resolves(&self, id: &str) -> bool {
        matches!(
            self.classify(id),
            PluginResolution::Named { resolvable: true }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtins_are_registered_under_their_gts_ids() {
        let registry = PluginRegistry::builtin();
        assert!(registry.auth(crate::domain::gts_helpers::BUILTIN_AUTH_NOOP).is_some());
        assert!(registry.auth(crate::domain::gts_helpers::BUILTIN_AUTH_APIKEY).is_some());
        assert!(
            registry
                .auth(crate::domain::gts_helpers::BUILTIN_AUTH_OAUTH2_CC)
                .is_some()
        );
        assert!(
            registry
                .guard(crate::domain::gts_helpers::BUILTIN_GUARD_REQUIRED_HEADERS)
                .is_some()
        );
        assert!(
            registry
                .transform(crate::domain::gts_helpers::BUILTIN_TRANSFORM_REQUEST_ID)
                .is_some()
        );
    }

    #[test]
    fn an_unknown_plugin_id_resolves_to_nothing() {
        let registry = PluginRegistry::builtin();
        assert!(registry.auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nope.v1").is_none());
        assert!(registry.guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nope.v1").is_none());
        assert!(registry.transform(uuid::Uuid::new_v4().to_string().as_str()).is_none());
    }
}
