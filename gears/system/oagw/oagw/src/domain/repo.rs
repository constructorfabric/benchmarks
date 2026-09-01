//! Repository traits for OAGW managed configuration.

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{PluginRecord, RouteRecord, UpstreamRecord};

/// Storage for upstreams, routes and custom plugins.
///
/// All lookups are tenant-scoped: the control plane never reads a sibling
/// tenant's configuration through these methods.
#[async_trait]
pub trait OagwRepository: Send + Sync {
    // -- upstreams ----------------------------------------------------------
    /// Insert an upstream; fails with [`DomainError::Conflict`] when `(tenant,
    /// alias)` is already taken.
    async fn insert_upstream(
        &self,
        tenant_id: Uuid,
        upstream: UpstreamRecord,
    ) -> Result<(), DomainError>;

    /// Update an upstream by id; fails with [`DomainError::NotFound`] when the
    /// caller does not own it, and [`DomainError::Conflict`] when the new
    /// alias collides with another of the caller's upstreams.
    async fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        upstream: UpstreamRecord,
    ) -> Result<(), DomainError>;

    /// Delete an upstream by id (caller-owned only).
    async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Fetch an upstream by id (caller-owned only).
    async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRecord, DomainError>;

    /// List the caller's upstreams.
    async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<UpstreamRecord>, DomainError>;

    /// List every upstream owned by any tenant in `tenant_ids` (proxy lookup).
    async fn list_upstreams_for_tenants(
        &self,
        tenant_ids: &[Uuid],
    ) -> Result<Vec<UpstreamRecord>, DomainError>;

    // -- routes -------------------------------------------------------------
    /// Insert a route; fails with [`DomainError::Conflict`] when the owning
    /// upstream already has a matching rule with the same priority.
    async fn insert_route(&self, record: RouteRecord) -> Result<(), DomainError>;

    /// Update a route by id (caller-owned only).
    async fn update_route(&self, tenant_id: Uuid, record: RouteRecord) -> Result<(), DomainError>;

    /// Delete a route by id (caller-owned only).
    async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Fetch a route by id (caller-owned only).
    async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRecord, DomainError>;

    /// List the caller's routes.
    async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<RouteRecord>, DomainError>;

    /// List routes owned by `tenant_id` for a specific upstream.
    async fn list_routes_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<RouteRecord>, DomainError>;

    /// List every route owned by any tenant in `tenant_ids`.
    async fn list_routes_for_tenants(
        &self,
        tenant_ids: &[Uuid],
    ) -> Result<Vec<RouteRecord>, DomainError>;

    // -- plugins ------------------------------------------------------------
    /// Insert a custom plugin (uuid-collision safe: new ids are v4).
    async fn insert_plugin(&self, record: PluginRecord) -> Result<(), DomainError>;

    /// Delete a custom plugin by id (caller-owned only).
    async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Fetch a custom plugin by id (caller-owned only).
    async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRecord, DomainError>;

    /// List the caller's custom plugins.
    async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<PluginRecord>, DomainError>;

    /// Whether any of the caller's upstreams or routes references `plugin_id`
    /// (bare uuid or GTS id).
    async fn plugin_is_referenced(
        &self,
        tenant_id: Uuid,
        plugin_key: &str,
    ) -> Result<bool, DomainError>;
}
