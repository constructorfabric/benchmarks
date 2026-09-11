//! Repository traits for the control plane's configuration.
//!
//! The domain is written against these traits only; the in-process
//! implementation lives in [`crate::infra::memory_repo`] and a database-backed
//! one can replace it without touching the domain.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{PluginBinding, Route, Upstream};

/// Persistence for upstreams, scoped by owning tenant.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Insert a new upstream. Errors when the alias is already taken in scope.
    ///
    /// # Errors
    /// Returns [`crate::domain::error::ErrorKind::AliasConflict`] when the
    /// alias collides with an existing upstream visible to the tenant.
    async fn insert(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Fetch an upstream by id within a tenant.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError>;

    /// Fetch an upstream by lowercase alias, searching `tenant_id` first and
    /// then each ancestor in order.
    async fn get_by_alias(
        &self,
        scope: &[Uuid],
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError>;

    /// List upstreams visible to `tenant_id` — its own plus inheritable ones
    /// from ancestors.
    async fn list_visible(&self, scope: &[Uuid]) -> Result<Vec<Upstream>, DomainError>;

    /// Replace an existing upstream, leaving `alias` and `created_at` intact.
    async fn update(&self, upstream: &Upstream) -> Result<(), DomainError>;

    /// Delete an upstream by id within a tenant.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}

/// Persistence for routes, scoped by owning tenant.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Insert a new route.
    async fn insert(&self, route: &Route) -> Result<(), DomainError>;

    /// Fetch a route by id within a tenant.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError>;

    /// List the tenant's own routes.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// List the tenant's enabled routes — the routing candidate set.
    async fn list_matching(
        &self,
        tenant_id: Uuid,
        method: &str,
        path: &str,
    ) -> Result<Vec<Route>, DomainError>;

    /// Routes whose `target_alias` resolves to `alias`.
    async fn routes_referencing_alias(
        &self,
        scope: &[Uuid],
        alias: &str,
    ) -> Result<Vec<Route>, DomainError>;

    /// Replace an existing route.
    async fn update(&self, route: &Route) -> Result<(), DomainError>;

    /// Delete a route by id within a tenant, returning whether it existed.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError>;
}

/// Persistence for plugin bindings and plugin lifecycle state.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Record a plugin binding on a route.
    async fn bind(
        &self,
        tenant_id: Uuid,
        route_id: Uuid,
        binding: &PluginBinding,
    ) -> Result<(), DomainError>;

    /// Remove a plugin binding.
    async fn unbind(
        &self,
        tenant_id: Uuid,
        route_id: Uuid,
        plugin_id: &str,
    ) -> Result<(), DomainError>;

    /// Bindings of `plugin_id` in scope.
    async fn list_bindings(
        &self,
        scope: &[Uuid],
        plugin_id: &str,
    ) -> Result<Vec<(Uuid, PluginBinding)>, DomainError>;
}
