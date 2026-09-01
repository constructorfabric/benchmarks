//! Repository abstraction for OAGW configuration storage.
//!
//! The MVP persists configuration in-process (per-tenant, tenant-isolated).
//! The trait keeps the service layer storage-agnostic (a DB-backed
//! implementation can slot in later).  All operations are synchronous —
//! the in-memory store guards state with a `parking_lot::RwLock`.

use uuid::Uuid;

use super::dto::{CustomPlugin, Route, Upstream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteOutcome {
    Deleted,
    NotFound,
}

/// Storage for all OAGW managed resources, keyed by tenant scope.
pub trait OagwRepository: Send + Sync {
    // ---- upstreams ----
    fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream>;
    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream>;
    /// Find upstreams by alias in a single tenant scope.
    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream>;
    fn insert_upstream(&self, upstream: Upstream);
    fn update_upstream(&self, upstream: Upstream);
    fn remove_upstream(&self, tenant_id: Uuid, id: Uuid) -> DeleteOutcome;

    // ---- routes ----
    fn list_routes(&self, tenant_id: Uuid) -> Vec<Route>;
    fn list_routes_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Route>;
    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route>;
    fn insert_route(&self, route: Route);
    fn update_route(&self, route: Route);
    fn remove_route(&self, tenant_id: Uuid, id: Uuid) -> DeleteOutcome;

    // ---- custom plugins ----
    fn list_plugins(&self, tenant_id: Uuid) -> Vec<CustomPlugin>;
    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<CustomPlugin>;
    fn insert_plugin(&self, plugin: CustomPlugin);
    fn remove_plugin(&self, tenant_id: Uuid, id: Uuid) -> DeleteOutcome;

    /// Resolve the upstream that currently references `plugin_ref` (used to
    /// determine effective plugin chains at proxy time).
    fn upstream_plugin_refs(&self, tenant_id: Uuid) -> Vec<(Uuid, String, Option<Uuid>)>;
}
