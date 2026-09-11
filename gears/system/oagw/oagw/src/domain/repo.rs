//! Repository contracts.
//!
//! Repositories are synchronous: the control plane is in-process
//! (`research.md` R5) and the underlying state is a `dashmap`/`parking_lot`
//! structure, so no `async` boundary is crossed below the service layer.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};

/// Tenant-scoped upstream persistence.
pub trait UpstreamRepository: Send + Sync {
    /// Inserts an upstream, enforcing per-tenant alias uniqueness.
    ///
    /// # Errors
    /// [`DomainError::AliasConflict`] when another upstream in the tenant
    /// already holds the normalized alias.
    fn insert(&self, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Looks up an upstream by id within one tenant.
    fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;

    /// Looks up an upstream by normalized alias within one tenant.
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;

    /// Resolves an alias across a tenant chain, closest tenant first.
    fn find_in_chain(&self, chain: &[Uuid], alias: &str) -> Option<Upstream>;

    /// Lists a tenant's upstreams, oldest first.
    fn list(&self, tenant_id: Uuid) -> Vec<Upstream>;

    /// Replaces an upstream in place.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the id is unknown to the tenant.
    fn replace(&self, tenant_id: Uuid, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Deletes an upstream.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the id is unknown to the tenant.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;

    /// Deletes every route of an upstream; returns how many were removed.
    fn delete_routes_of(&self, tenant_id: Uuid, upstream_id: Uuid) -> usize;
}

/// Tenant-scoped route persistence.
pub trait RouteRepository: Send + Sync {
    /// Inserts a route.
    ///
    /// # Errors
    /// [`DomainError::DuplicateMatchRule`] when the match rule collides with
    /// an existing route on the same upstream.
    fn insert(&self, route: Route) -> Result<Route, DomainError>;

    /// Looks up a route by id within one tenant.
    fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;

    /// Lists a tenant's routes, oldest first.
    fn list(&self, tenant_id: Uuid) -> Vec<Route>;

    /// Lists a tenant's routes for one upstream, oldest first.
    fn list_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route>;

    /// Replaces a route in place, keeping its id and upstream.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the id is unknown to the tenant.
    fn replace(&self, tenant_id: Uuid, route: Route) -> Result<Route, DomainError>;

    /// Deletes a route.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the id is unknown to the tenant.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;
}

/// Tenant-scoped custom plugin persistence.
pub trait PluginRepository: Send + Sync {
    /// Inserts a plugin.
    ///
    /// # Errors
    /// [`DomainError::Validation`] when `(tenant, kind, name)` already exists.
    fn insert(&self, plugin: Plugin) -> Result<Plugin, DomainError>;

    /// Looks up a plugin by id within one tenant.
    fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin>;

    /// Lists a tenant's plugins, oldest first.
    fn list(&self, tenant_id: Uuid) -> Vec<Plugin>;

    /// Deletes a plugin.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the id is unknown to the tenant.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;
}

/// Aggregate view of the control-plane state, for the cross-resource checks the
/// individual repositories cannot express on their own.
pub trait ControlPlane: Send + Sync {
    /// Upstream state.
    fn upstreams(&self) -> &dyn UpstreamRepository;

    /// Route state.
    fn routes(&self) -> &dyn RouteRepository;

    /// Custom-plugin state.
    fn plugins(&self) -> &dyn PluginRepository;

    /// `true` when any upstream or route of the tenant references the plugin.
    fn plugin_in_use(&self, tenant_id: Uuid, plugin_id: Uuid) -> bool;

    /// Deletes an upstream and cascades to its routes.
    ///
    /// # Errors
    /// [`DomainError::NotFound`] when the id is unknown to the tenant.
    fn delete_upstream_cascade(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<(Upstream, usize), DomainError>;
}
