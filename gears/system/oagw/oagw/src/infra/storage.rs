//! Tenant-scoped in-memory stores for the OAGW control plane.
//!
//! # DESIGN-led deviation
//!
//! The DESIGN calls for `SeaORM` + `toolkit-db` persistence (`oagw_upstream`,
//! `oagw_route`, `oagw_plugin` tables). The crate manifest carries no `SeaORM` /
//! `toolkit-db` dependency, so — per the documented deviations — the control
//! plane is implemented on an in-memory store with the *same* semantics:
//! tenant-scoped CRUD, unique `(tenant_id, alias)`, immutable alias after
//! creation, immutable route `upstream_id`, delete-in-use → 409 for plugins,
//! and upstream deletion cascades to the routes bound to it.

use crate::domain::models::{PluginRecord, Route, Upstream};
use dashmap::{DashMap, mapref::one::RefMut};
use uuid::Uuid;

/// The full in-memory control-plane store.
#[derive(Debug, Default)]
pub struct OagwStore {
    /// `tenant_id -> (upstream_id -> Upstream)`.
    upstreams: DashMap<Uuid, DashMap<Uuid, Upstream>>,
    /// `tenant_id -> (route_id -> Route)`.
    routes: DashMap<Uuid, DashMap<Uuid, Route>>,
    /// `tenant_id -> (plugin_id -> PluginRecord)`.
    plugins: DashMap<Uuid, DashMap<Uuid, PluginRecord>>,
    /// `tenant_id -> (alias -> upstream_id)` (uniqueness index).
    aliases: DashMap<Uuid, DashMap<String, Uuid>>,
}

impl OagwStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mutable tenant-scoped upstream table (created on demand). Reads through
    /// the returned handle are fine; the entry is always materialized.
    #[must_use]
    pub fn upstreams(&self, tenant_id: Uuid) -> RefMut<'_, Uuid, DashMap<Uuid, Upstream>> {
        self.upstreams.entry(tenant_id).or_default()
    }

    /// Mutable tenant-scoped route table (created on demand).
    #[must_use]
    pub fn routes(&self, tenant_id: Uuid) -> RefMut<'_, Uuid, DashMap<Uuid, Route>> {
        self.routes.entry(tenant_id).or_default()
    }

    /// Mutable tenant-scoped plugin table (created on demand).
    #[must_use]
    pub fn plugins(&self, tenant_id: Uuid) -> RefMut<'_, Uuid, DashMap<Uuid, PluginRecord>> {
        self.plugins.entry(tenant_id).or_default()
    }

    /// Mutable tenant-scoped alias index (created on demand).
    #[must_use]
    pub fn aliases(&self, tenant_id: Uuid) -> RefMut<'_, Uuid, DashMap<String, Uuid>> {
        self.aliases.entry(tenant_id).or_default()
    }
}
