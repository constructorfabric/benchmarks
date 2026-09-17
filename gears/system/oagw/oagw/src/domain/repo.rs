//! Repository traits for the OAGW control plane.
//!
//! Repositories are tenant-scoped: every lookup takes the calling tenant and
//! returns only resources owned by that tenant (ancestor resources are 404
//! via the management API). The in-memory implementation lives in
//! `crate::infra::storage`.

use std::sync::Arc;

use uuid::Uuid;

use super::models::{Plugin, Route, Upstream};

/// Repository for upstream resources.
pub trait UpstreamRepo: Send + Sync {
    /// Insert or replace an upstream owned by `tenant_id`.
    fn upsert(&self, tenant_id: Uuid, u: Upstream) -> Result<(), anyhow::Error>;

    /// Delete an upstream owned by `tenant_id`. Returns `true` if deleted.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error>;

    /// Get an upstream owned by `tenant_id`.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>>;

    /// List all upstreams owned by `tenant_id`.
    fn list(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>>;

    /// Whether an alias is already taken by another upstream owned by
    /// `tenant_id` (excluding `except_id`).
    fn alias_taken(&self, tenant_id: Uuid, alias: &str, except_id: Option<Uuid>) -> bool;
}

/// Repository for route resources.
pub trait RouteRepo: Send + Sync {
    /// Insert or replace a route owned by `tenant_id`.
    fn upsert(&self, tenant_id: Uuid, r: Route) -> Result<(), anyhow::Error>;

    /// Delete a route owned by `tenant_id`. Returns `true` if deleted.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error>;

    /// Get a route owned by `tenant_id`.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>>;

    /// List all routes owned by `tenant_id`.
    fn list(&self, tenant_id: Uuid) -> Vec<Arc<Route>>;

    /// List routes owned by `tenant_id` that target `upstream_id`.
    fn list_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>>;
}

/// Repository for custom plugin resources.
pub trait PluginRepo: Send + Sync {
    /// Insert a plugin owned by `tenant_id`.
    fn put(&self, tenant_id: Uuid, p: Plugin) -> Result<(), anyhow::Error>;

    /// Delete a plugin owned by `tenant_id`. Returns `true` if deleted.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, anyhow::Error>;

    /// Get a plugin owned by `tenant_id`.
    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Plugin>>;

    /// List all plugins owned by `tenant_id`.
    fn list(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>>;
}
