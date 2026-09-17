//! Repository port.
//!
//! The domain depends on this trait only; `crate::infra::memory` provides the
//! in-memory implementation this configuration ships with. The port is
//! synchronous on purpose — a management-plane mutation is short and local —
//! and it is *one* trait rather than three narrow ones because several
//! operations are cross-collection by contract (`DESIGN.md` "Key
//! Invariants": multi-table updates are atomic): deleting an upstream
//! cascades its routes, and deleting a plugin must observe both collections
//! at the same instant.
//!
//! All operations are tenant-scoped by construction: a caller may never
//! observe another tenant's rows.
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};

/// Read-only view of a tenant's configuration, handed to the data plane
/// (dispatch 2) so it can build its own routing snapshot. Cloning yields a
/// cheap, consistent point-in-time copy.
#[derive(Debug, Clone, Default)]
pub struct ControlPlaneSnapshot {
    /// All upstreams of the tenant.
    pub upstreams: Vec<Upstream>,
    /// All routes of the tenant.
    pub routes: Vec<Route>,
}

/// Persistence port for the OAGW control plane.
pub trait ConfigStore: Send + Sync {
    // -- upstreams ----------------------------------------------------------

    /// Insert a new upstream. Fails with [`DomainError::Conflict`] when the
    /// id or the `(tenant_id, alias)` pair already exists.
    ///
    /// # Errors
    ///
    /// [`DomainError::Conflict`].
    fn insert_upstream(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Fetch one upstream of `tenant` by id.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn find_upstream(&self, tenant: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Fetch one upstream of `tenant` by its normalized alias.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn find_upstream_by_alias(
        &self,
        tenant: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError>;

    /// Replace an existing upstream in place, keeping its `created_at`.
    /// Returns the previous version, or `None` when it no longer exists.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn replace_upstream(&self, upstream: &Upstream) -> Result<Option<Upstream>, DomainError>;

    /// Remove an upstream, returning the removed row.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn delete_upstream(&self, tenant: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Delete an upstream *and* every route bound to it in one critical
    /// section (`oagw_route.upstream_id` is `ON DELETE CASCADE`). Returns the
    /// removed upstream and the number of cascaded routes.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn delete_upstream_cascade(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<(Option<Upstream>, usize), DomainError>;

    /// List every upstream of a tenant, unordered.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn list_upstreams(&self, tenant: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// `true` when another upstream of `tenant` owns `alias`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn upstream_alias_taken(
        &self,
        tenant: Uuid,
        alias: &str,
        except_id: Option<Uuid>,
    ) -> Result<bool, DomainError>;

    // -- routes -------------------------------------------------------------

    /// Insert a new route. Fails with [`DomainError::Conflict`] when the id
    /// already exists or the route's match key is already claimed.
    ///
    /// # Errors
    ///
    /// [`DomainError::Conflict`].
    fn insert_route(&self, route: &Route) -> Result<(), DomainError>;

    /// Fetch one route of `tenant` by id.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn find_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// Replace an existing route in place, re-indexing its match key.
    /// Returns the previous version, or `None` when it no longer exists.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn replace_route(&self, route: &Route) -> Result<Option<Route>, DomainError>;

    /// Remove one route and return it.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// List every route of a tenant, unordered.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn list_routes(&self, tenant: Uuid) -> Result<Vec<Route>, DomainError>;

    /// List the routes of one upstream.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn list_routes_by_upstream(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError>;

    /// `true` when another route of the same upstream already claims the
    /// `(upstream_id, priority, match key)` tuple.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn route_match_key_taken(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
        match_key: &str,
        except_id: Option<Uuid>,
    ) -> Result<bool, DomainError>;

    // -- plugins ------------------------------------------------------------

    /// Insert a new custom plugin. Fails with [`DomainError::Conflict`] when
    /// the id or the `(tenant_id, name)` pair already exists.
    ///
    /// # Errors
    ///
    /// [`DomainError::Conflict`].
    fn insert_plugin(&self, plugin: &Plugin) -> Result<(), DomainError>;

    /// Fetch one plugin of `tenant` by id.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn find_plugin(&self, tenant: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// Fetch one plugin of `tenant` by its unique name.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn find_plugin_by_name(&self, tenant: Uuid, name: &str) -> Result<Option<Plugin>, DomainError>;

    /// Remove one plugin and return it.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn delete_plugin(&self, tenant: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError>;

    /// List every plugin of a tenant, unordered.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn list_plugins(&self, tenant: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// Collect every upstream and route that references `plugin_id`, either
    /// through its auth plugin or through a chain binding, in one snapshot.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`].
    fn plugin_references(
        &self,
        tenant: Uuid,
        plugin_id: Uuid,
    ) -> Result<crate::domain::error::PluginReferences, DomainError>;
}
