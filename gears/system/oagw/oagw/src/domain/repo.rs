//! Repository ports for the OAGW control plane.
//!
//! The domain services depend on these traits only; the in-memory
//! implementation used in this phase lives in [`crate::infra::storage::memory`]
//! and a persistent backend can be swapped in behind the same contracts.
//!
//! All lookups are strictly tenant-scoped: the management API never sees
//! ancestor resources (DESIGN.md §3.4, "Tenant Scoping").

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Route, Upstream};

/// Persistence port for upstreams.
pub trait UpstreamRepository: Send + Sync {
    /// Inserts a new upstream; the alias must still be unique.
    ///
    /// # Errors
    ///
    /// [`DomainError::AliasConflict`] when `(tenant_id, alias)` is taken.
    fn insert(&self, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Loads an upstream by id, scoped to `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Loads an upstream by alias, scoped to `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Upstream>, DomainError>;

    /// Lists every upstream of `tenant_id` in creation order.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Replaces an existing upstream (id + tenant must match an existing row).
    ///
    /// # Errors
    ///
    /// [`DomainError::AliasConflict`] on an alias collision,
    /// [`DomainError::NotFound`] when the row disappeared.
    fn replace(&self, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Deletes an upstream; returns `false` when it does not exist.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Id of the upstream holding `alias` in `tenant_id`, if any.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn alias_owner(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Uuid>, DomainError>;
}

/// Persistence port for routes.
pub trait RouteRepository: Send + Sync {
    /// Inserts a new route.
    ///
    /// # Errors
    ///
    /// [`DomainError::DuplicateRouteMatch`] when the match rules collide.
    fn insert(&self, route: Route) -> Result<Route, DomainError>;

    /// Loads a route by id, scoped to `tenant_id`.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// Lists every route of `tenant_id` in creation order.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Lists every route of one upstream, in creation order.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn list_by_upstream(&self, upstream_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Replaces an existing route.
    ///
    /// # Errors
    ///
    /// [`DomainError::DuplicateRouteMatch`] when the match rules collide,
    /// [`DomainError::NotFound`] when the row disappeared.
    fn replace(&self, route: Route) -> Result<Route, DomainError>;

    /// Deletes a route; returns `false` when it does not exist.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;

    /// Removes every route pointing at `upstream_id` (upstream deletion).
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    fn delete_by_upstream(&self, upstream_id: Uuid) -> Result<usize, DomainError>;
}

/// Bundled repository pair handed to the services.
#[derive(Clone)]
pub struct Repositories {
    /// Upstream persistence.
    pub upstreams: std::sync::Arc<dyn UpstreamRepository>,
    /// Route persistence.
    pub routes: std::sync::Arc<dyn RouteRepository>,
}

impl Repositories {
    /// Builds a bundle from two concrete repositories.
    #[must_use]
    pub fn new(
        upstreams: std::sync::Arc<dyn UpstreamRepository>,
        routes: std::sync::Arc<dyn RouteRepository>,
    ) -> Self {
        Self { upstreams, routes }
    }
}

impl std::fmt::Debug for Repositories {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repositories").finish_non_exhaustive()
    }
}
