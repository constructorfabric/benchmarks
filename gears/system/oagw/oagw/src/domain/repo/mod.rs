//! Repository contracts for the OAGW control plane.
//!
//! The gear is `stateful` but **not** `db`: persistence is an in-memory store
//! (`infra::storage`) behind these traits, so a real backend can be swapped in
//! without touching the services or the REST layer.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, RouteConfig, Upstream, UpstreamConfig};
use crate::domain::error::DomainError;

/// Query parameters shared by every list endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// OData `$filter` (only `alias eq '...'` is honoured for upstreams).
    pub filter: Option<String>,
    /// OData `$search` (substring match on the alias / name / tags).
    pub search: Option<String>,
    /// OData `$top`.
    pub top: Option<usize>,
    /// OData `$skip`.
    pub skip: Option<usize>,
    /// OData `$orderby` (`alias desc`, `created_at`, …).
    pub orderby: Option<String>,
}

/// Result envelope for a paged listing.
#[derive(Debug, Clone, Default)]
pub struct Page<T> {
    /// Matching rows.
    pub items: Vec<T>,
    /// Total number of matching rows before paging.
    pub total: usize,
}

/// Upsert outcome, used to distinguish `201` from `200`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutated {
    /// The resource did not exist before.
    Created,
    /// An existing resource was replaced.
    Replaced,
}

impl Mutated {
    /// `true` for a create.
    #[must_use]
    pub fn is_created(self) -> bool {
        matches!(self, Self::Created)
    }
}

/// Store of [`Upstream`] resources, scoped by tenant.
#[async_trait]
pub trait UpstreamRepo: Send + Sync {
    /// Persist a new upstream; fails when the id or the alias is taken.
    async fn create(&self, upstream: Upstream) -> Result<Upstream, DomainError>;

    /// Replace an upstream in place (id and tenant are immutable).
    async fn update(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        config: UpstreamConfig,
    ) -> Result<Upstream, DomainError>;

    /// Fetch one upstream by tenant and id.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;

    /// Fetch one upstream by tenant and routing alias.
    async fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError>;

    /// Remove an upstream; fails when routes still reference it.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// List upstreams of a tenant.
    async fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Page<Upstream>, DomainError>;

    /// Number of routes pointing at this upstream.
    async fn route_count(&self, tenant_id: Uuid, id: Uuid) -> Result<usize, DomainError>;

    /// Every upstream using `alias`, regardless of tenant.
    ///
    /// Used only by the CORS preflight path, which the edge middleware serves
    /// with an anonymous context (browsers never send credentials on a
    /// preflight). It returns configuration objects, never secrets.
    async fn find_by_alias_any(&self, alias: &str) -> Result<Vec<Upstream>, DomainError>;
}

/// Store of [`Route`] resources, scoped by tenant.
#[async_trait]
pub trait RouteRepo: Send + Sync {
    /// Persist a new route; fails on id reuse or a match-rule collision.
    async fn create(&self, route: Route) -> Result<Route, DomainError>;

    /// Replace a route in place (id, tenant and upstream are immutable).
    async fn update(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        config: RouteConfig,
    ) -> Result<Route, DomainError>;

    /// Fetch one route by tenant and id.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;

    /// Remove a route.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// List routes of a tenant.
    async fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Page<Route>, DomainError>;

    /// List routes that target a given upstream.
    async fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError>;

    /// Find the route whose match rules win for `(method, path, query)`.
    async fn find_match(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Option<Route>, DomainError>;
}

/// Store of tenant-defined [`Plugin`] resources.
#[async_trait]
pub trait PluginRepo: Send + Sync {
    /// Persist a new plugin; fails when the name is taken in the tenant.
    async fn create(&self, plugin: Plugin) -> Result<Plugin, DomainError>;

    /// Fetch one plugin by tenant and id.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;

    /// Remove a plugin; fails when an upstream or route still references it.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// List plugins of a tenant.
    async fn list(&self, tenant_id: Uuid, query: &ListQuery)
        -> Result<Page<Plugin>, DomainError>;

    /// Count upstreams and routes referencing this plugin.
    async fn reference_count(&self, tenant_id: Uuid, id: Uuid) -> Result<usize, DomainError>;
}
