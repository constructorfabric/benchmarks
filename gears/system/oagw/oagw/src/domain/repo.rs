//! Repository ports. The domain layer talks to storage only through these
//! traits; `infra::storage` provides the shipped implementation.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{Plugin, Route, Upstream};

/// Tenant-scoped upstream persistence.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream.
    ///
    /// # Errors
    ///
    /// Returns a `409` when `(tenant_id, alias)` is already taken.
    async fn insert(&self, upstream: Upstream) -> Result<Upstream, OagwError>;

    /// Replace an existing upstream wholesale.
    ///
    /// # Errors
    ///
    /// Returns a `404` when the upstream does not exist for that tenant.
    async fn replace(&self, upstream: Upstream) -> Result<Upstream, OagwError>;

    /// Fetch one upstream owned by `tenant_id`.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;

    /// Fetch one upstream owned by `tenant_id` and carrying `alias`.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;

    /// All upstreams owned by `tenant_id`, ordered by creation time.
    async fn list(&self, tenant_id: Uuid) -> Vec<Upstream>;

    /// Delete one upstream; `true` when a row was removed.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;

    /// Every upstream, across tenants — used only to answer
    /// "is this plugin still referenced?".
    async fn all(&self) -> Vec<Upstream>;
}

/// Tenant-scoped route persistence.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    ///
    /// Returns a `409` when an enabled sibling already claims the same
    /// `(path, priority, method)` triple.
    async fn insert(&self, route: Route) -> Result<Route, OagwError>;

    /// Replace an existing route wholesale.
    ///
    /// # Errors
    ///
    /// Returns a `404` when the route does not exist for that tenant, or a
    /// `409` on a match-rule collision.
    async fn replace(&self, route: Route) -> Result<Route, OagwError>;

    /// Fetch one route owned by `tenant_id`.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;

    /// All routes owned by `tenant_id`.
    async fn list(&self, tenant_id: Uuid) -> Vec<Route>;

    /// All routes under `upstream_id`, regardless of tenant.
    async fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route>;

    /// Delete one route; `true` when a row was removed.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;

    /// Delete every route under `upstream_id` (cascade on upstream delete).
    async fn delete_by_upstream(&self, upstream_id: Uuid) -> usize;

    /// Every route, across tenants — used only for plugin reference checks.
    async fn all(&self) -> Vec<Route>;
}

/// Tenant-scoped custom plugin persistence. Plugins are immutable, so there
/// is no replace.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin.
    ///
    /// # Errors
    ///
    /// Returns a `409` when `(tenant_id, name)` is already taken.
    async fn insert(&self, plugin: Plugin) -> Result<Plugin, OagwError>;

    /// Fetch one plugin owned by `tenant_id`.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin>;

    /// All plugins owned by `tenant_id`.
    async fn list(&self, tenant_id: Uuid) -> Vec<Plugin>;

    /// Delete one plugin; `true` when a row was removed.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;

    /// Record that the plugin was resolved on the hot path.
    async fn touch(&self, id: Uuid);

    /// Mark unlinked plugins GC-eligible and delete those already past their
    /// `gc_eligible_at`. Returns the number of deleted rows.
    async fn collect_garbage(&self, linked: &[Uuid], ttl_days: u64) -> usize;
}
