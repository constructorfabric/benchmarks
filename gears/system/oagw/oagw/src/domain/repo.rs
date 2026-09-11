//! Repository contracts for the Control Plane.
//!
//! The methods are synchronous because the shipped implementation
//! ([`crate::infra::storage`]) is an in-process store; the trait boundary is
//! what keeps a future persistent backend a drop-in.

use uuid::Uuid;

use crate::domain::error::OagwResult;
use crate::domain::model::{PluginDef, Route, Upstream};

/// Storage for [`Upstream`] aggregates.
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream. Fails with a conflict when `(tenant_id, alias)`
    /// is taken.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when the alias is already used by the tenant.
    fn insert(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Replace an existing upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when the upstream is absent or owned by
    /// another tenant.
    fn replace(&self, upstream: Upstream) -> OagwResult<Upstream>;

    /// Fetch one upstream, scoped to its owning tenant.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;

    /// Fetch one upstream regardless of tenant — Data Plane use only.
    fn get_unscoped(&self, id: Uuid) -> Option<Upstream>;

    /// All upstreams owned by `tenant_id`.
    fn list(&self, tenant_id: Uuid) -> Vec<Upstream>;

    /// The upstream owned by `tenant_id` under `alias`, if any.
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;

    /// Remove an upstream owned by `tenant_id`; `true` when something was
    /// removed.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;
}

/// Storage for [`Route`] aggregates.
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when an equivalent match rule already exists.
    fn insert(&self, route: Route) -> OagwResult<Route>;

    /// Replace an existing route owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns a not-found error when the route is absent or owned by another
    /// tenant.
    fn replace(&self, route: Route) -> OagwResult<Route>;

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;

    fn list(&self, tenant_id: Uuid) -> Vec<Route>;

    /// Every route bound to `upstream_id`, regardless of tenant.
    fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route>;

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;

    /// Drop every route bound to `upstream_id` (cascade on upstream delete).
    fn delete_by_upstream(&self, upstream_id: Uuid) -> usize;
}

/// Storage for tenant-defined [`PluginDef`] rows.
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin definition.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when `(tenant_id, name)` is taken.
    fn insert(&self, plugin: PluginDef) -> OagwResult<PluginDef>;

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginDef>;

    fn get_unscoped(&self, id: Uuid) -> Option<PluginDef>;

    fn list(&self, tenant_id: Uuid) -> Vec<PluginDef>;

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool;

    /// Mark a plugin as used at `epoch_secs` (feeds the GC clock).
    fn touch(&self, id: Uuid, epoch_secs: u64);

    /// Set or clear the instant after which an unlinked plugin is collectable.
    fn set_gc_eligible_at(&self, id: Uuid, epoch_secs: Option<u64>);
}
