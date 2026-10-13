//! Lazy plugin resolution through types-registry
//! (`cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin`,
//! `cpt-cf-mini-chat-state-gear-foundation-plugin-resolution`).
//!
//! The registry and the client hub are reached through two small traits so the
//! resolution logic is independent of both.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;

/// Bound of one types-registry query.
pub const REGISTRY_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// A plugin instance registered in types-registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInstance {
    /// GTS instance id.
    pub id: String,
    /// Vendor of the instance.
    pub vendor: String,
    /// Priority; lower wins.
    pub priority: i16,
}

/// Lists the registered instances of one plugin spec.
#[async_trait]
pub trait PluginInstanceSource: Send + Sync {
    /// Returns every registered instance of the spec.
    ///
    /// # Errors
    ///
    /// Returns the failure text of the registry query.
    async fn list_instances(&self) -> Result<Vec<PluginInstance>, String>;
}

/// Looks up the client registered for an instance id.
pub trait PluginClientLookup<C: ?Sized>: Send + Sync {
    /// Returns the client scoped to `instance_id`, if registered.
    fn client(&self, instance_id: &str) -> Option<Arc<C>>;
}

/// Plugin resolution failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PluginResolutionError {
    /// The registry query failed.
    #[error("types-registry query failed: {0}")]
    RegistryQuery(String),
    /// The registry query exceeded its bound.
    #[error("types-registry query timed out")]
    RegistryTimeout,
    /// The selected instance has no client in the hub.
    #[error("plugin client missing for instance `{instance_id}`")]
    ClientMissing {
        /// Instance id without a client.
        instance_id: String,
    },
}

/// Resolves and caches the plugin client of one spec for the gear vendor.
///
/// State machine (`cpt-cf-mini-chat-state-gear-foundation-plugin-resolution`):
/// `unresolved` (nothing cached) and `resolved` (the instance id is cached). The
/// cache holds the instance id only; the client is looked up on every call, so a
/// client that disappears sends the resolver back to `unresolved`.
pub struct PluginResolver<C: ?Sized> {
    vendor: String,
    source: Arc<dyn PluginInstanceSource>,
    clients: Arc<dyn PluginClientLookup<C>>,
    registry_timeout: Duration,
    cached: Mutex<Option<String>>,
}

impl<C: ?Sized + Send + Sync> PluginResolver<C> {
    /// Creates a resolver with the default `REGISTRY_QUERY_TIMEOUT`.
    #[must_use]
    pub fn new(
        vendor: impl Into<String>,
        source: Arc<dyn PluginInstanceSource>,
        clients: Arc<dyn PluginClientLookup<C>>,
    ) -> Self {
        Self {
            vendor: vendor.into(),
            source,
            clients,
            registry_timeout: REGISTRY_QUERY_TIMEOUT,
            cached: Mutex::new(None),
        }
    }

    /// Overrides the registry query bound.
    #[must_use]
    pub fn with_registry_timeout(mut self, timeout: Duration) -> Self {
        self.registry_timeout = timeout;
        self
    }

    /// The cached instance id (state `resolved`), if any.
    #[must_use]
    pub fn cached_instance_id(&self) -> Option<String> {
        self.cache().clone()
    }

    /// Resolves the client; `Ok(None)` means "no plugin registered".
    ///
    /// # Errors
    ///
    /// Returns [`PluginResolutionError`] when the registry query fails or times
    /// out, or when the selected instance has no client.
    pub async fn resolve(&self) -> Result<Option<Arc<C>>, PluginResolutionError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-if-cached
        let cached = self.cached_instance_id();
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-if-cached
        let instance_id = if let Some(instance_id) = cached {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-skip-to-client
            instance_id
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-skip-to-client
        } else {
            let Some(instance_id) = self.select_instance().await? else {
                return Ok(None);
            };
            instance_id
        };

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-get-client
        let client = self.clients.client(&instance_id);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-get-client
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-if-client-absent
        let Some(client) = client else {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-reset-cache
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-plugin-resolution:p1:inst-to-unresolved
            *self.cache() = None;
            return Err(PluginResolutionError::ClientMissing { instance_id });
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-plugin-resolution:p1:inst-to-unresolved
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-reset-cache
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-if-client-absent

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-return-client
        Ok(Some(client))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-return-client
    }

    /// Queries the registry and picks the instance of the gear vendor with the
    /// lowest priority value (ties broken by instance id ascending); caches its
    /// id. `Ok(None)` when no instance matches (nothing is cached).
    async fn select_instance(&self) -> Result<Option<String>, PluginResolutionError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-query-registry
        let instances =
            match tokio::time::timeout(self.registry_timeout, self.source.list_instances()).await {
                Ok(Ok(instances)) => instances,
                Ok(Err(message)) => return Err(PluginResolutionError::RegistryQuery(message)),
                Err(_elapsed) => return Err(PluginResolutionError::RegistryTimeout),
            };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-query-registry

        // The toolkit selector breaks ties by first seen; sorting by
        // `(priority, id)` makes the choice independent of the registry order.
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-else-instance-found
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-select-instance
        let selected = instances
            .into_iter()
            .filter(|instance| instance.vendor == self.vendor)
            .min_by(|a, b| (a.priority, &a.id).cmp(&(b.priority, &b.id)));
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-select-instance
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-else-instance-found

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-if-none
        let Some(selected) = selected else {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-return-none
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-plugin-resolution:p1:inst-stay-unresolved
            return Ok(None);
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-plugin-resolution:p1:inst-stay-unresolved
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-return-none
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin:p1:inst-if-none

        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-plugin-resolution:p1:inst-to-resolved
        *self.cache() = Some(selected.id.clone());
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-plugin-resolution:p1:inst-to-resolved
        Ok(Some(selected.id))
    }

    /// The cache never holds an invariant across a panic, so a poisoned lock is
    /// still usable.
    fn cache(&self) -> MutexGuard<'_, Option<String>> {
        self.cached.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "plugin_resolution_tests.rs"]
mod tests;
