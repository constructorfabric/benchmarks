//! Repository contracts for the in-memory configuration store.
//!
//! All lookups are tenant-scoped: management operations only ever see the
//! calling tenant's own records (DESIGN "Tenant Scoping").

use uuid::Uuid;

use crate::domain::error::OagwResult;
use crate::domain::model::{Plugin, Route, Upstream};

/// Upstream storage.
pub trait UpstreamRepository: Send + Sync {
    /// Inserts a new upstream.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn insert_upstream(&self, upstream: Upstream) -> OagwResult<()>;

    /// Fetches an upstream by id (any tenant).
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn find_upstream(&self, id: uuid::Uuid) -> OagwResult<Option<Upstream>>;

    /// Fetches an upstream by `(tenant_id, alias)`.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn find_upstream_by_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> OagwResult<Option<Upstream>>;

    /// All upstreams owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn upstreams_for_tenant(&self, tenant_id: uuid::Uuid) -> OagwResult<Vec<Upstream>>;

    /// Replaces the stored upstream record.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn update_upstream(&self, upstream: Upstream) -> OagwResult<()>;

    /// Deletes an upstream by id.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn delete_upstream(&self, id: uuid::Uuid) -> OagwResult<()>;

    /// Number of routes referencing `upstream_id`.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn upstream_route_count(&self, id: uuid::Uuid) -> OagwResult<usize>;
}

/// Route storage.
pub trait RouteRepository: Send + Sync {
    /// Inserts a new route.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn insert_route(&self, route: Route) -> OagwResult<()>;

    /// Fetches a route by id (any tenant).
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn find_route(&self, id: uuid::Uuid) -> OagwResult<Option<Route>>;

    /// All routes owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn routes_for_tenant(&self, tenant_id: uuid::Uuid) -> OagwResult<Vec<Route>>;

    /// All routes for an upstream across all tenants (data-plane matching).
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn routes_for_upstream(&self, upstream_id: uuid::Uuid) -> OagwResult<Vec<Route>>;

    /// Replaces the stored route record.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn update_route(&self, route: Route) -> OagwResult<()>;

    /// Deletes a route by id.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn delete_route(&self, id: uuid::Uuid) -> OagwResult<()>;
}

/// Plugin storage (custom plugins only; built-ins are catalogued elsewhere).
pub trait PluginRepository: Send + Sync {
    /// Inserts a new plugin.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn insert_plugin(&self, plugin: Plugin) -> OagwResult<()>;

    /// Fetches a plugin by id (any tenant).
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn find_plugin(&self, id: &str) -> OagwResult<Option<Plugin>>;

    /// All plugins owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn plugins_for_tenant(&self, tenant_id: uuid::Uuid) -> OagwResult<Vec<Plugin>>;

    /// Deletes a plugin by id.
    ///
    /// # Errors
    ///
    /// Implementation-defined storage failure.
    fn delete_plugin(&self, id: &str) -> OagwResult<()>;
}

/// Mutations that must invalidate the data-plane L1 config cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigGeneration(pub u64);

/// Shared generation counter bumped on every control-plane mutation so the
/// data plane can drop stale L1 entries (ADR-0005).
#[derive(Debug, Default)]
pub struct ConfigGenerationCounter(parking_lot::RwLock<u64>);

impl ConfigGenerationCounter {
    /// Current generation.
    #[must_use]
    pub fn current(&self) -> u64 {
        *self.0.read()
    }

    /// Bumps the generation and returns the new value.
    pub fn bump(&self) -> u64 {
        let mut guard = self.0.write();
        *guard += 1;
        *guard
    }
}

/// Alias used for readability at call sites.
pub type GenerationRef = std::sync::Arc<ConfigGenerationCounter>;

/// Convenience: empty id set used by callers building "not found" errors.
#[must_use]
pub fn nil_id() -> Uuid {
    Uuid::nil()
}
