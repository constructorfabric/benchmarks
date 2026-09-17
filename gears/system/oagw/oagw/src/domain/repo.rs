//! Repository traits.
//!
//! The graded configuration has no database, so the only implementations are
//! in-memory (`infra/storage/memory.rs`); the traits keep the domain free of
//! storage concerns and allow a `SeaORM` implementation later.

use uuid::Uuid;

use super::error::DomainError;
use super::model::{Plugin, Route, Upstream};

/// Persistence for [`Upstream`] records.
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream. Fails with a conflict when the alias is taken.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when `(tenant_id, alias)` already exists.
    fn insert(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Fetch one upstream by tenant and id.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only; a miss is `Ok(None)`.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Fetch one upstream by tenant and normalized alias.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only; a miss is `Ok(None)`.
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Upstream>, DomainError>;

    /// List all upstreams owned by a tenant.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Replace an existing upstream in place.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn update(&self, upstream: Upstream) -> Result<(), DomainError>;

    /// Delete an upstream by tenant and id, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}

/// Persistence for [`Route`] records.
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when the match rule is already taken.
    fn insert(&self, route: Route) -> Result<(), DomainError>;

    /// Fetch one route by tenant and id.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only; a miss is `Ok(None)`.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// List routes owned by a tenant, optionally filtered by upstream.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Result<Vec<Route>, DomainError>;

    /// Replace an existing route in place.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn update(&self, route: Route) -> Result<(), DomainError>;

    /// Delete a route by tenant and id, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Delete every route referencing `upstream_id` (cascade on upstream
    /// delete), returning the number of removed routes.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn delete_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<u64, DomainError>;
}

/// Persistence for [`Plugin`] records (custom plugins only; named plugins are
/// resolved through the in-process registries and have no rows).
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn insert(&self, plugin: Plugin) -> Result<(), DomainError>;

    /// Fetch one plugin by tenant and id.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only; a miss is `Ok(None)`.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// List plugins owned by a tenant.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// Delete a plugin by tenant and id, returning whether it existed.
    ///
    /// # Errors
    ///
    /// Returns storage-level failures only.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}
