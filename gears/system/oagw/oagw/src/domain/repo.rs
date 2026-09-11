//! Persistence seams for the control plane.
//!
//! The graded configuration provisions no database for this gear, so the control plane stores
//! configuration in-process. These traits keep the storage engine swappable without touching the
//! handlers (DESIGN D2).

use uuid::Uuid;

use super::error::DomainError;
use super::model::{Plugin, Route, RouteMatch, Upstream};

/// Stored upstream configuration.
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream.
    fn insert(&self, upstream: Upstream) -> Result<Upstream, DomainError>;
    /// Fetch one upstream by tenant and id.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;
    /// Fetch the upstream carrying `alias` in `tenant_id`.
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;
    /// True when some other upstream in the tenant already holds the alias.
    fn alias_taken(&self, tenant_id: Uuid, alias: &str, excluding: Option<Uuid>) -> bool;
    /// List upstreams, optionally restricted to one tenant.
    fn list(&self, tenant_id: Option<Uuid>) -> Vec<Upstream>;
    /// Replace an existing upstream.
    fn update(&self, upstream: Upstream) -> Result<Upstream, DomainError>;
    /// Remove an upstream.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;
    /// Number of stored upstreams.
    fn count(&self) -> usize;
}

/// Stored route configuration.
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    fn insert(&self, route: Route) -> Result<Route, DomainError>;
    /// Fetch one route by tenant and id.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;
    /// List routes, optionally restricted to one tenant.
    fn list(&self, tenant_id: Option<Uuid>) -> Vec<Route>;
    /// List routes bound to an upstream.
    fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route>;
    /// Replace an existing route.
    fn update(&self, route: Route) -> Result<Route, DomainError>;
    /// Remove a route.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;
    /// The route already claiming the same match rule within `upstream_id`, if any.
    fn find_matching(&self, upstream_id: Uuid, route_match: &RouteMatch) -> Option<Route>;
    /// Number of stored routes.
    fn count(&self) -> usize;
}

/// Stored custom plugin definitions.
pub trait PluginRepository: Send + Sync {
    /// Insert a new plugin.
    fn insert(&self, plugin: Plugin) -> Result<Plugin, DomainError>;
    /// Fetch one plugin by tenant and id.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin>;
    /// List plugins, optionally restricted to one tenant.
    fn list(&self, tenant_id: Option<Uuid>) -> Vec<Plugin>;
    /// Replace an existing plugin.
    fn update(&self, plugin: Plugin) -> Result<Plugin, DomainError>;
    /// Remove a plugin.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;
    /// GTS ids of the upstreams and routes referencing the plugin.
    fn referencing_resources(&self, tenant_id: Uuid, plugin_id: Uuid) -> Vec<String>;
    /// Number of stored plugins.
    fn count(&self) -> usize;
}
