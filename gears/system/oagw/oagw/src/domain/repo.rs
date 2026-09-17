//! Repository traits for the control plane.
//!
//! The contract is intentionally narrow so the backing implementation is
//! swappable; OAGW ships an in-process store (see
//! `crate::infra::store::InMemoryStore`). Implementations must enforce tenant
//! scoping themselves — ancestor resources are invisible to the management API.

use async_trait::async_trait;

use super::error::DomainResult;
use super::model::{Plugin, Route, Upstream};

/// Filter applied to list operations. `None` means "no constraint".
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    /// Exact alias match (normalized).
    pub alias: Option<String>,
    /// Restrict to an owning upstream.
    pub upstream_id: Option<uuid::Uuid>,
    /// Restrict to a plugin kind.
    pub plugin_type: Option<String>,
    /// Maximum number of rows.
    pub top: Option<usize>,
    /// Number of rows to skip.
    pub skip: Option<usize>,
}

impl ListFilter {
    /// Binds the filter to `top` and `skip` with the documented defaults.
    #[must_use]
    pub fn page(&self) -> (usize, usize) {
        let top = self.top.unwrap_or(50).min(100);
        let skip = self.skip.unwrap_or(0);
        (top, skip)
    }
}

/// Upstream persistence contract.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Inserts a new upstream, rejecting a duplicate `(tenant_id, alias)`.
    ///
    /// # Errors
    ///
    /// Returns a 409 `PluginInUse`-shaped conflict error on alias collision.
    async fn insert(&self, upstream: Upstream) -> DomainResult<Upstream>;

    /// Fetches one upstream owned by `tenant_id`.
    async fn find_by_id(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Option<Upstream>>;

    /// Fetches one upstream by normalized alias in a single tenant.
    async fn find_by_alias(
        &self,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> DomainResult<Option<Upstream>>;

    /// Lists upstreams owned by `tenant_id`.
    async fn list(&self, tenant_id: uuid::Uuid, filter: &ListFilter)
    -> DomainResult<Vec<Upstream>>;

    /// Replaces an existing upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns 404 when absent, 409 when the alias collides with a sibling.
    async fn update(&self, upstream: Upstream) -> DomainResult<Upstream>;

    /// Deletes an upstream owned by `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns 409 when routes still reference the upstream.
    async fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()>;
}

/// Route persistence contract.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Inserts a route.
    ///
    /// # Errors
    ///
    /// Returns 409 when an identical match rule already exists for the
    /// upstream.
    async fn insert(&self, route: Route) -> DomainResult<Route>;

    /// Fetches one route owned by `tenant_id`.
    async fn find_by_id(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Option<Route>>;

    /// Lists routes owned by `tenant_id`, optionally filtered by upstream.
    async fn list(&self, tenant_id: uuid::Uuid, filter: &ListFilter) -> DomainResult<Vec<Route>>;

    /// Lists every route in the tenant chain, used by the data plane.
    async fn list_by_tenants(
        &self,
        tenant_ids: &[uuid::Uuid],
        filter: &ListFilter,
    ) -> DomainResult<Vec<Route>>;

    /// Replaces an existing route owned by `tenant_id`.
    async fn update(&self, route: Route) -> DomainResult<Route>;

    /// Deletes a route owned by `tenant_id`.
    async fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()>;
}

/// Plugin persistence contract.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Inserts an immutable plugin definition.
    async fn insert(&self, plugin: Plugin) -> DomainResult<Plugin>;

    /// Fetches one plugin owned by `tenant_id`.
    async fn find_by_id(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Option<Plugin>>;

    /// Lists plugins owned by `tenant_id`.
    async fn list(&self, tenant_id: uuid::Uuid, filter: &ListFilter) -> DomainResult<Vec<Plugin>>;

    /// Deletes an unlinked plugin.
    ///
    /// # Errors
    ///
    /// Returns 409 when the plugin is still referenced by an upstream or route.
    async fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()>;

    /// Marks the plugin eligible for garbage collection.
    async fn mark_gc_eligible(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
        at_millis: i64,
    ) -> DomainResult<()>;
}
