//! Repository traits for the OAGW control plane.
//!
//! The data plane and control plane talk to these traits only, so the
//! storage backend is swappable (see `DESIGN.md §3.2` DDD-Light layering).

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::model::{Plugin, Route, Upstream};

/// Read/write access to tenant-scoped upstream configuration.
#[async_trait]
pub trait UpstreamRepository: Send + Sync {
    /// Persist a new upstream.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn insert(&self, upstream: Upstream) -> Result<(), anyhow::Error>;

    /// Replace an existing upstream.
    ///
    /// # Errors
    /// Returns an error when the upstream does not exist or storage fails.
    async fn update(&self, upstream: Upstream) -> Result<(), anyhow::Error>;

    /// Fetch one upstream by identifier, scoped to `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, anyhow::Error>;

    /// Fetch the enabled upstream owning `alias` in `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn find_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, anyhow::Error>;

    /// List all upstreams owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, anyhow::Error>;

    /// Delete an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error>;

    /// `true` when another upstream in `tenant_id` already owns `alias`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn alias_taken(
        &self,
        tenant_id: Uuid,
        alias: &str,
        exclude_id: Option<Uuid>,
    ) -> Result<bool, anyhow::Error>;
}

/// Read/write access to routes.
#[async_trait]
pub trait RouteRepository: Send + Sync {
    /// Persist a new route.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn insert(&self, route: Route) -> Result<(), anyhow::Error>;

    /// Replace an existing route.
    ///
    /// # Errors
    /// Returns an error when the route does not exist or storage fails.
    async fn update(&self, route: Route) -> Result<(), anyhow::Error>;

    /// Fetch one route by identifier, scoped to `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, anyhow::Error>;

    /// List routes owned by `tenant_id`, optionally filtered to one upstream.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>)
        -> Result<Vec<Route>, anyhow::Error>;

    /// Delete a route owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error>;
}

/// Read/write access to tenant-defined custom plugins.
#[async_trait]
pub trait PluginRepository: Send + Sync {
    /// Persist a new custom plugin.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn insert(&self, plugin: Plugin) -> Result<(), anyhow::Error>;

    /// Fetch one plugin by identifier, scoped to `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, anyhow::Error>;

    /// List plugins owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, anyhow::Error>;

    /// Delete a plugin owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns an error when the storage layer fails.
    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error>;
}
