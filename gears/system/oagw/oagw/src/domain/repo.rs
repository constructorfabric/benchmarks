// Created: 2026-08-29 by Constructor Tech
//! Repository contracts for the control plane.
//!
//! The MVP store is in-memory (`infra/storage.rs`); the traits keep the domain
//! layer persistence-agnostic so a database-backed implementation can replace
//! it without touching the services.

use uuid::Uuid;

use super::error::OagwError;
use super::model::{PluginDefinition, Route, Upstream};

/// `UNIQUE (tenant_id, alias)` violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasConflict {
    /// Conflicting alias.
    pub alias: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
}

/// Persistence for upstreams.
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `(tenant_id, alias)` already exists.
    fn insert(&self, upstream: Upstream) -> Result<Upstream, OagwError>;

    /// Replace an existing upstream.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the id is unknown, and
    /// [`OagwError::Validation`] when the replacement breaks a uniqueness
    /// constraint.
    fn update(&self, upstream: Upstream) -> Result<Upstream, OagwError>;

    /// Fetch an upstream by id.
    #[must_use]
    fn get(&self, id: Uuid) -> Option<Upstream>;

    /// Fetch an upstream by `(tenant_id, alias)`, case-insensitive.
    #[must_use]
    fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;

    /// All upstreams owned by `tenant_id`.
    #[must_use]
    fn list_by_tenant(&self, tenant_id: Uuid) -> Vec<Upstream>;

    /// Every upstream matching `alias` across all tenants, case-insensitive.
    #[must_use]
    fn list_by_alias(&self, alias: &str) -> Vec<Upstream>;

    /// Every upstream, across all tenants.
    #[must_use]
    fn list_all(&self) -> Vec<Upstream>;

    /// Delete an upstream by id.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the id is unknown.
    fn delete(&self, id: Uuid) -> Result<(), OagwError>;

    /// Count upstreams.
    #[must_use]
    fn count(&self) -> usize;
}

/// Persistence for routes.
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when a matching route already exists.
    fn insert(&self, route: Route) -> Result<Route, OagwError>;

    /// Replace an existing route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the id is unknown.
    fn update(&self, route: Route) -> Result<Route, OagwError>;

    /// Fetch a route by id.
    #[must_use]
    fn get(&self, id: Uuid) -> Option<Route>;

    /// Every route owned by `tenant_id`.
    #[must_use]
    fn list_by_tenant(&self, tenant_id: Uuid) -> Vec<Route>;

    /// Every route bound to `upstream_id`.
    #[must_use]
    fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route>;

    /// Every route, across all tenants.
    #[must_use]
    fn list_all(&self) -> Vec<Route>;

    /// Delete a route by id.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the id is unknown.
    fn delete(&self, id: Uuid) -> Result<(), OagwError>;

    /// Count routes.
    #[must_use]
    fn count(&self) -> usize;
}

/// Persistence for custom (Starlark) plugins.
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the name is taken.
    fn insert(&self, plugin: PluginDefinition) -> Result<PluginDefinition, OagwError>;

    /// Fetch a plugin by id.
    #[must_use]
    fn get(&self, id: Uuid) -> Option<PluginDefinition>;

    /// Every plugin owned by `tenant_id`.
    #[must_use]
    fn list_by_tenant(&self, tenant_id: Uuid) -> Vec<PluginDefinition>;

    /// Every plugin, across all tenants.
    #[must_use]
    fn list_all(&self) -> Vec<PluginDefinition>;

    /// Delete a plugin by id.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the id is unknown.
    fn delete(&self, id: Uuid) -> Result<(), OagwError>;

    /// Count plugins.
    #[must_use]
    fn count(&self) -> usize;
}
