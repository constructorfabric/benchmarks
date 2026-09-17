//! Repository traits of the oagw control plane.
//!
//! Repositories are keyed by `(tenant_id, id)`: every read and write is
//! tenant-scoped, so an ancestor resource is invisible to a descendant (and
//! vice versa) through this layer. Hierarchical visibility is implemented by
//! `ControlPlaneService` on top of the tenant chain, not here.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};

/// Persistence of upstream configurations.
pub trait UpstreamRepository: std::fmt::Debug + Send + Sync {
    /// Stores a new upstream. Fails with [`ErrorKind::Conflict`] when the
    /// `(tenant_id, alias)` pair is already taken.
    ///
    /// # Errors
    /// Returns [`DomainError`] for alias conflicts.
    fn insert(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Replaces an existing upstream in place.
    ///
    /// # Errors
    /// Returns [`DomainError`] for alias conflicts and missing resources.
    fn update(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Looks an upstream up by id, scoped to the tenant.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Looks an upstream up by alias, scoped to the tenant.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Upstream>, DomainError>;

    /// Lists the upstreams of one tenant, ordered by alias.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Deletes an upstream; returns whether it existed.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Every upstream of every tenant, for control-plane wide scans (plugin
    /// in-use tracking, alias shadowing).
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn list_all(&self) -> Result<Vec<Upstream>, DomainError>;
}

/// Persistence of route configurations.
pub trait RouteRepository: std::fmt::Debug + Send + Sync {
    /// Stores a new route.
    ///
    /// # Errors
    /// Returns [`DomainError`] on match conflicts.
    fn insert(&self, route: &Route) -> Result<(), DomainError>;

    /// Replaces an existing route in place.
    ///
    /// # Errors
    /// Returns [`DomainError`] on match conflicts.
    fn update(&self, route: &Route) -> Result<(), DomainError>;

    /// Looks a route up by id, scoped to the tenant.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// Lists the routes of one tenant, optionally filtered by upstream.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Result<Vec<Route>, DomainError>;

    /// Deletes a route; returns whether it existed.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Every route of every tenant, for control-plane wide scans.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn list_all(&self) -> Result<Vec<Route>, DomainError>;
}

/// Persistence of custom (Starlark) plugin definitions.
pub trait PluginRepository: std::fmt::Debug + Send + Sync {
    /// Stores a new plugin.
    ///
    /// # Errors
    /// Returns [`DomainError`] for duplicate names.
    fn insert(&self, plugin: &Plugin) -> Result<(), DomainError>;

    /// Looks a plugin up by id, scoped to the tenant.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// Lists the plugins of one tenant.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// Deletes a plugin; returns whether it existed.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}
