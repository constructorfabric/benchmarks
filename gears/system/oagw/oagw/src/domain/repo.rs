//! Repository traits for OAGW resources.
//!
//! Implementations are in-memory (`crate::infra::storage::memory`) — the
//! trait boundary keeps the domain independent of the storage backend.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, TenantId, Upstream};

/// Upstream repository (tenant-scoped reads/writes).
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream.
    ///
    /// # Errors
    ///
    /// - `DomainError::AliasViolation` when `(tenant_id, alias)` already
    ///   exists.
    fn insert(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Replace an existing upstream owned by `tenant`.
    ///
    /// # Errors
    ///
    /// - `DomainError::NotFound` when the row does not exist for the tenant.
    /// - `DomainError::AliasViolation` on alias collision with another row.
    fn replace(&self, tenant: TenantId, upstream: Upstream) -> Result<(), DomainError>;

    /// Fetch the tenant's own upstream by id.
    fn get_by_id(&self, tenant: TenantId, id: Uuid) -> Option<Upstream>;

    /// Fetch the tenant's own upstream by normalized alias (case-insensitive).
    fn get_by_alias(&self, tenant: TenantId, alias: &str) -> Option<Upstream>;

    /// Whether the tenant already has an upstream with this alias.
    fn alias_taken(&self, tenant: TenantId, alias: &str) -> bool;

    /// List the tenant's own upstreams.
    fn list(&self, tenant: TenantId) -> Vec<Upstream>;

    /// Delete the tenant's own upstream by id.
    ///
    /// # Errors
    ///
    /// `DomainError::NotFound` when the row does not exist (tenant-scoped).
    fn delete(&self, tenant: TenantId, id: Uuid) -> Result<(), DomainError>;
}

/// Route repository (tenant-scoped reads/writes).
pub trait RouteRepository: Send + Sync {
    fn insert(&self, route: Route) -> Result<(), DomainError>;
    fn get_by_id(&self, tenant: TenantId, id: Uuid) -> Option<Route>;
    fn list(&self, tenant: TenantId) -> Vec<Route>;
    /// Routes of `tenant` bound to a given upstream.
    fn list_for_upstream(&self, tenant: TenantId, upstream_id: Uuid) -> Vec<Route>;
    fn replace(&self, tenant: TenantId, route: Route) -> Result<(), DomainError>;
    fn delete(&self, tenant: TenantId, id: Uuid) -> Result<(), DomainError>;
    /// Whether any route (any tenant) references the upstream — used to guard
    /// upstream deletion (cascade-per-tenant: only the owning tenant's routes
    /// matter for scoped deletion).
    fn any_route_for_upstream(&self, upstream_id: Uuid) -> bool;
}

/// Custom-plugin repository (tenant-scoped).
pub trait PluginRepository: Send + Sync {
    fn insert(&self, plugin: Plugin) -> Result<(), DomainError>;
    fn get_by_id(&self, tenant: TenantId, id: Uuid) -> Option<Plugin>;
    fn get_by_name(&self, tenant: TenantId, name: &str) -> Option<Plugin>;
    fn list(&self, tenant: TenantId) -> Vec<Plugin>;
    fn delete(&self, tenant: TenantId, id: Uuid) -> Result<(), DomainError>;
    /// Mark an unlinked plugin as garbage-collection-eligible after its TTL.
    fn mark_gc_eligible(&self, tenant: TenantId, id: Uuid, eligible_at: u64) -> Result<(), DomainError>;
}

