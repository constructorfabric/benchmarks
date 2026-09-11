//! Repository contracts of the OAGW gear.
//!
//! The contracts are declared by this feature and implemented by `infra`
//! (in-memory store, DECOMPOSITION assumption 3). The domain layer depends on
//! these traits only — never on a storage type — so the management service
//! (`cpt-cf-oagw-algo-sharing-mode-validate`, `cpt-cf-oagw-algo-odata-query`)
//! is written against them and `infra` supplies the implementation.
//!
//! Every contract is keyed by `(tenant_id, id)`: the calling tenant's key space
//! is the only one a repository call can see, which is what makes an
//! ancestor-owned record indistinguishable from a missing one
//! (`cpt-cf-oagw-dod-tenant-scoping`).

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::ManagementError;
use crate::domain::model::{Plugin, Route, Upstream};

// @cpt-begin:cpt-cf-oagw-dod-store-invariants:p1:inst-full
/// Storage contract for the upstream entity and its dependent rows.
///
/// The implementation (`cpt-cf-oagw-algo-store-invariants`) enforces the write
/// invariants — `UNIQUE (tenant_id, alias)`, contiguous plugin positions, one
/// atomic write over the record and its dependent rows — inside its own
/// critical section, so a caller never observes a partial write.
pub trait UpstreamRepository: Send + Sync {
    /// Insert an upstream with its tag and plugin rows.
    ///
    /// # Errors
    ///
    /// Returns the store's rejection when the alias key is already taken in the
    /// tenant (`409`) or a plugin-position invariant is violated (`400`).
    fn insert_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, ManagementError>;

    /// Replace an upstream, keeping `id`, `tenant_id` and `alias` immutable.
    ///
    /// # Errors
    ///
    /// Returns the store's rejection when the replacement is not the stored
    /// identity or a plugin-position invariant is violated.
    fn replace_upstream(
        &self,
        existing: &Upstream,
        replacement: Upstream,
    ) -> Result<Arc<Upstream>, ManagementError>;

    /// Delete an upstream and cascade its route rows.
    ///
    /// Returns the routes the cascade removed, so the caller can report the
    /// cascade; the caller's `404` decision is made before this is reached.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::not_found`] when the record is unknown.
    fn delete_upstream(&self, tenant_id: Uuid, id: Uuid)
    -> Result<Vec<Arc<Route>>, ManagementError>;

    /// The upstream of one tenant carrying `id`, or `None` when the tenant's
    /// key space does not hold it.
    fn find_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>>;

    /// The upstream of one tenant carrying `alias`, or `None`.
    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>>;

    /// Every upstream of one tenant.
    fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>>;
}

/// Storage contract for the route entity and its dependent rows.
///
/// Declared alongside [`UpstreamRepository`] by this feature: a route owns its
/// own match, method, tag and plugin rows, so its write contract is separate
/// from the upstream's even though a route always references one upstream of
/// the same tenant.
pub trait RouteRepository: Send + Sync {
    /// Insert a route with its match, method, tag and plugin rows.
    ///
    /// # Errors
    ///
    /// Returns the store's rejection when `upstream_id` does not resolve inside
    /// the tenant, the match rule collides with another enabled route (`409`)
    /// or a plugin-position invariant is violated (`400`).
    fn insert_route(&self, route: Route) -> Result<Arc<Route>, ManagementError>;

    /// Replace a route, keeping `id`, `tenant_id` and `upstream_id` immutable.
    ///
    /// # Errors
    ///
    /// Returns the store's rejection when the replacement is not the stored
    /// identity, the revalidated match rule collides or the plugin positions
    /// are not contiguous.
    fn replace_route(
        &self,
        existing: &Route,
        replacement: Route,
    ) -> Result<Arc<Route>, ManagementError>;

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::not_found`] when the record is unknown.
    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), ManagementError>;

    /// The route of one tenant carrying `id`, or `None`.
    fn find_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>>;

    /// Every route of one tenant.
    fn list_routes(&self, tenant_id: Uuid) -> Vec<Arc<Route>>;

    /// Every route of one tenant that references `upstream_id`.
    fn list_routes_of_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>>;
}
// @cpt-end:cpt-cf-oagw-dod-store-invariants:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-plugin-store-invariants:p1:inst-full
/// Storage contract for the plugin definition entity
/// (`cpt-cf-oagw-algo-plugin-store-write`).
///
/// `oagw_plugin` is keyed by `id` under `UNIQUE (tenant_id, name)`, so the
/// implementation enforces the uniqueness inside the same critical section as
/// the write, applies the change as one atomic unit and publishes the new
/// immutable snapshot before returning. A named builtin plugin has no row, so it
/// never resolves through this contract.
pub trait PluginRepository: Send + Sync {
    /// Insert a definition, enforcing `UNIQUE (tenant_id, name)`.
    ///
    /// # Errors
    ///
    /// Returns a `409` when another definition of the same tenant already holds
    /// the `(tenant_id, name)` key.
    fn insert_plugin(&self, plugin: Plugin) -> Result<Arc<Plugin>, ManagementError>;

    /// Delete a definition after the in-use scan
    /// (`cpt-cf-oagw-algo-plugin-in-use-scan`), inside the same critical
    /// section, so no binding can start referencing it between the check and
    /// the write.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::not_found`] when the record is unknown and a
    /// `409` carrying `plugin_id` and `referenced_by` when a live reference
    /// remains.
    fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> Result<(), ManagementError>;

    /// The definition of one tenant carrying `id`, or `None`.
    ///
    /// `id` is the GTS identifier of the definition.
    fn find_plugin(&self, tenant_id: Uuid, id: &str) -> Option<Arc<Plugin>>;

    /// The definition of one tenant keyed by the UUID instance of its
    /// identifier, or `None`.
    fn find_plugin_by_uuid(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Plugin>>;

    /// Every definition of one tenant.
    fn list_plugins(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>>;
}
// @cpt-end:cpt-cf-oagw-dod-plugin-store-invariants:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;

    /// The contracts are object-safe so `infra` can provide them as trait
    /// objects and the management service can hold them behind a single type.
    #[allow(dead_code)]
    type BoxedUpstreams = Box<dyn UpstreamRepository>;
    #[allow(dead_code)]
    type BoxedRoutes = Box<dyn RouteRepository>;
    #[allow(dead_code)]
    type BoxedPlugins = Box<dyn PluginRepository>;

    #[test]
    fn upstream_repository_contract_is_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn UpstreamRepository>();
    }

    #[test]
    fn route_repository_contract_is_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn RouteRepository>();
    }

    #[test]
    fn plugin_repository_contract_is_object_safe() {
        fn assert_object_safe<T: ?Sized>() {}
        assert_object_safe::<dyn PluginRepository>();
    }

    #[test]
    fn repository_contracts_are_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<dyn UpstreamRepository>();
        assert_send_sync::<dyn RouteRepository>();
        assert_send_sync::<dyn PluginRepository>();
    }
}
