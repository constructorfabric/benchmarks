//! Repository contracts.
//!
//! The Control Plane owns configuration data through these traits. The
//! shipped implementation is in-process ([`crate::infra::storage`]); a
//! SeaORM-backed one would satisfy the same contracts without touching the
//! domain layer.

use async_trait::async_trait;
use uuid::Uuid;

use super::error::DomainResult;
use super::model::{Plugin, Route, Upstream};

/// Tenant-scoped storage for upstreams.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream.
    ///
    /// # Errors
    ///
    /// `409` when `(tenant_id, alias)` already exists.
    async fn create(&self, upstream: Upstream) -> DomainResult<Upstream>;

    /// Replace an existing upstream wholesale.
    ///
    /// # Errors
    ///
    /// `404` when the upstream is not owned by the caller's tenant.
    async fn replace(&self, upstream: Upstream) -> DomainResult<Upstream>;

    /// Fetch by id within a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Upstream>>;

    /// Fetch by `(tenant_id, alias)`; the alias is already normalized.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> DomainResult<Option<Upstream>>;

    /// List every upstream owned by a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list(&self, tenant_id: Uuid) -> DomainResult<Vec<Upstream>>;

    /// Delete by id within a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool>;

    /// Every upstream in the store, across tenants — used to answer
    /// "is this plugin still in use?".
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn all(&self) -> DomainResult<Vec<Upstream>>;
}

/// Tenant-scoped storage for routes.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    ///
    /// `409` when an enabled route on the same upstream already claims the
    /// same `(path, priority, method)`.
    async fn create(&self, route: Route) -> DomainResult<Route>;

    /// Replace an existing route wholesale.
    ///
    /// # Errors
    ///
    /// `404` when the route is not owned by the caller's tenant.
    async fn replace(&self, route: Route) -> DomainResult<Route>;

    /// Fetch by id within a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Route>>;

    /// List every route owned by a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list(&self, tenant_id: Uuid) -> DomainResult<Vec<Route>>;

    /// List routes attached to an upstream, regardless of owning tenant —
    /// proxy-time matching inherits ancestor routes.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list_by_upstream(&self, upstream_id: Uuid) -> DomainResult<Vec<Route>>;

    /// Delete by id within a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool>;

    /// Delete every route belonging to an upstream (cascade).
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn delete_by_upstream(&self, upstream_id: Uuid) -> DomainResult<usize>;

    /// Every route in the store, across tenants.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn all(&self) -> DomainResult<Vec<Route>>;
}

/// Tenant-scoped storage for custom plugin definitions.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin definition.
    ///
    /// # Errors
    ///
    /// `409` when `(tenant_id, name)` already exists.
    async fn create(&self, plugin: Plugin) -> DomainResult<Plugin>;

    /// Fetch by id within a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Plugin>>;

    /// Fetch by id regardless of tenant — proxy-time binding resolution may
    /// reference an ancestor's plugin.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn get_any(&self, id: Uuid) -> DomainResult<Option<Plugin>>;

    /// List every plugin owned by a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn list(&self, tenant_id: Uuid) -> DomainResult<Vec<Plugin>>;

    /// Delete by id within a tenant.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool>;

    /// Record that `id` became unlinked at `at_epoch_secs`, or clear the
    /// marker when it is bound again.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn set_gc_eligible_at(&self, id: Uuid, at_epoch_secs: Option<u64>) -> DomainResult<()>;

    /// Delete every plugin whose `gc_eligible_at` is at or before `now`.
    ///
    /// # Errors
    ///
    /// Propagates storage failures.
    async fn collect_garbage(&self, now_epoch_secs: u64) -> DomainResult<usize>;
}
