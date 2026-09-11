//! In-memory entity storage (DECOMPOSITION assumption 3).
//!
//! DECOMPOSITION entry 2.2 implements [`crate::domain::repo`] here on the crate's
//! existing `dashmap` / `parking_lot` / `arc-swap` dependency set: no
//! `toolkit-db` dependency, no SQL and no migration surface
//! (`cpt-cf-oagw-constraint-multi-sql`).
//!
//! # Logical model
//!
//! The in-memory keys are the logical model of `cpt-cf-oagw-db-schema`, with the
//! DESIGN table and column names:
//!
//! | Logical row | Stored representation |
//! |---|---|
//! | `oagw_upstream` | [`Upstream`] keyed by `(tenant_id, id)` |
//! | `oagw_upstream_alias` (`UNIQUE (tenant_id, alias)`) | the alias index |
//! | `oagw_upstream_tag` | [`Upstream::tags`] |
//! | `oagw_upstream_plugin` `(position, plugin_ref, plugin_uuid)` | [`Upstream::plugins`] |
//! | `oagw_route` | [`Route`] keyed by `(tenant_id, id)` |
//! | `oagw_route_http_match` / `oagw_route_grpc_match` | [`Route::matches`] |
//! | `oagw_route_method` | [`HttpMatch::methods`] |
//! | `oagw_route_tag` / `oagw_route_plugin` | [`Route::tags`] / [`Route::plugins`] |
//!
//! # Invariants (`cpt-cf-oagw-algo-store-invariants`)
//!
//! Every write takes the owning tenant's write lock, checks its invariants
//! inside that critical section, applies the whole change atomically and then
//! publishes a new immutable snapshot. A rejected invariant discards the staged
//! change and leaves no partial write behind.

// @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-08
// The logical model of `cpt-cf-oagw-db-schema` stays the store's contract, with
// the DESIGN table and column names — `oagw_plugin`, `oagw_upstream_plugin` and
// `oagw_route_plugin` among them — realized on the crate's existing
// `dashmap` / `parking_lot` / `arc-swap` dependency set: no `toolkit-db`
// dependency and no SQL, per DECOMPOSITION assumption 3.
// @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-08

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::RawMutex;
use parking_lot::lock_api::ArcMutexGuard;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::{DomainError, ManagementError, ManagementRejection};
use crate::domain::model::{HttpMatch, Plugin, Route, Upstream};

/// Key of an upstream record: the logical `UNIQUE` identity of `oagw_upstream`.
type UpstreamKey = (Uuid, Uuid);

/// Key of a route record: the logical `UNIQUE` identity of `oagw_route`.
type RouteKey = (Uuid, Uuid);

/// Index of the `UNIQUE (tenant_id, alias)` constraint.
type AliasKey = (Uuid, String);

/// Index of the routes that reference one upstream.
type UpstreamRoutesKey = (Uuid, Uuid);

/// Key of a plugin definition record: the logical primary key of `oagw_plugin`.
type PluginKey = (Uuid, String);

/// Index of the `UNIQUE (tenant_id, name)` constraint on `oagw_plugin`.
type PluginNameKey = (Uuid, String);

// @cpt-begin:cpt-cf-oagw-dod-store-invariants:p1:inst-full
/// Immutable view of every upstream and route the gear serves.
///
/// Readers take `Arc`s out of it, so a reader either sees the previous or the
/// new state and never an intermediate one.
#[derive(Debug, Clone, Default)]
pub struct ConfigSnapshot {
    /// Monotonic epoch, bumped by every published snapshot.
    pub epoch: u64,
    /// Upstream records keyed by `(tenant_id, id)`.
    pub upstreams: BTreeMap<UpstreamKey, Arc<Upstream>>,
    /// Route records keyed by `(tenant_id, id)`.
    pub routes: BTreeMap<RouteKey, Arc<Route>>,
    /// Plugin definition records keyed by `(tenant_id, id)`.
    pub plugins: BTreeMap<PluginKey, Arc<Plugin>>,
}

impl ConfigSnapshot {
    /// The upstreams of one tenant, in `id` order.
    #[must_use]
    pub fn upstreams_of(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        self.upstreams
            .range((tenant_id, Uuid::nil())..=(tenant_id, Uuid::max()))
            .map(|(_key, upstream)| Arc::clone(upstream))
            .collect()
    }

    /// The routes of one tenant, in `id` order.
    #[must_use]
    pub fn routes_of(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        self.routes
            .range((tenant_id, Uuid::nil())..=(tenant_id, Uuid::max()))
            .map(|(_key, route)| Arc::clone(route))
            .collect()
    }

    /// The plugin definitions of one tenant, in identifier order.
    #[must_use]
    pub fn plugins_of(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>> {
        self.plugins
            .iter()
            .filter(|(key, _plugin)| key.0 == tenant_id)
            .map(|(_key, plugin)| Arc::clone(plugin))
            .collect()
    }
}

// @cpt-end:cpt-cf-oagw-dod-store-invariants:p1:inst-full

/// In-memory store of the OAGW upstreams, routes and their dependent rows.
///
/// One instance lives for the lifetime of the gear and is shared by the
/// management service and (from entry 2.4) the data plane, which resolves from
/// [`Self::snapshot`].
pub struct OagwStore {
    /// `oagw_upstream` rows.
    upstreams: DashMap<UpstreamKey, Arc<Upstream>>,
    /// The `UNIQUE (tenant_id, alias)` index: alias to upstream `id`.
    alias_index: DashMap<AliasKey, Uuid>,
    /// `oagw_route` rows.
    routes: DashMap<RouteKey, Arc<Route>>,
    /// Routes of one upstream, for the cascade and the match invariant.
    upstream_routes: DashMap<UpstreamRoutesKey, Vec<Uuid>>,
    /// `oagw_plugin` rows, keyed by `(tenant_id, identifier)`.
    plugins: DashMap<PluginKey, Arc<Plugin>>,
    /// The `UNIQUE (tenant_id, name)` index of `oagw_plugin`: name to identifier.
    plugin_name_index: DashMap<PluginNameKey, String>,
    /// Write lock per tenant key (`inst-store-01`).
    tenant_locks: DashMap<Uuid, Arc<Mutex<()>>>,
    /// The published configuration snapshot.
    snapshot: ArcSwap<ConfigSnapshot>,
    /// Cache-invalidation counter (`inst-store-08`).
    invalidations: AtomicU64,
}

impl Default for OagwStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OagwStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            upstreams: DashMap::new(),
            alias_index: DashMap::new(),
            routes: DashMap::new(),
            upstream_routes: DashMap::new(),
            plugins: DashMap::new(),
            plugin_name_index: DashMap::new(),
            tenant_locks: DashMap::new(),
            snapshot: ArcSwap::from_pointee(ConfigSnapshot::default()),
            invalidations: AtomicU64::new(0),
        }
    }

    /// The currently published configuration snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<ConfigSnapshot> {
        self.snapshot.load_full()
    }

    /// The epoch of the published snapshot.
    #[must_use]
    pub fn snapshot_epoch(&self) -> u64 {
        self.snapshot.load().epoch
    }

    /// How many times consumer caches were invalidated.
    #[must_use]
    pub fn invalidation_count(&self) -> u64 {
        self.invalidations.load(Ordering::Acquire)
    }

    /// The upstream of one tenant carrying `id`, if any.
    #[must_use]
    pub fn find_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        self.upstreams.get(&(tenant_id, id)).map(|entry| Arc::clone(&entry))
    }

    /// The upstream of one tenant carrying `alias`, if any.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        let id = *self.alias_index.get(&(tenant_id, alias.to_string()))?;
        self.find_upstream(tenant_id, id)
    }

    /// Every upstream of one tenant, from the published snapshot.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        self.snapshot().upstreams_of(tenant_id)
    }

    /// The route of one tenant carrying `id`, if any.
    #[must_use]
    pub fn find_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>> {
        self.routes.get(&(tenant_id, id)).map(|entry| Arc::clone(&entry))
    }

    /// Every route of one tenant, from the published snapshot.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        self.snapshot().routes_of(tenant_id)
    }

    /// Every route of one tenant that references `upstream_id`.
    #[must_use]
    pub fn list_routes_of_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        let ids = self
            .upstream_routes
            .get(&(tenant_id, upstream_id))
            .map(|entry| entry.value().clone())
            .unwrap_or_default();
        ids.iter()
            .filter_map(|id| self.find_route(tenant_id, *id))
            .collect()
    }

    // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-01
    /// Insert an upstream, enforcing the write-side invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError`] when the `UNIQUE (tenant_id, alias)`
    /// constraint or the plugin-position invariant is violated.
    pub fn insert_upstream(&self, mut upstream: Upstream) -> Result<Arc<Upstream>, ManagementError> {
        let _guard = self.lock(upstream.tenant_id);
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-01

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-02
        self.assert_alias_free(upstream.tenant_id, &upstream.alias)?;
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-02

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-04
        assert_bindings_consistent(upstream.plugins.as_ref().map(|plugins| &plugins.items))?;
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-04

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-06
        // A create stamps `created_at`; the list contract orders on it.
        upstream.created_at = crate::domain::model::Timestamp::now();
        let record = Arc::new(upstream);
        let key = (record.tenant_id, record.id);
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-06

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-05
        // One atomic unit: the record and both dependent row sets.
        self.alias_index
            .insert((record.tenant_id, record.alias.clone()), record.id);
        self.upstreams.insert(key, Arc::clone(&record));
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-05

        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-02
        // `available` -> `in_use` for every definition the written binding rows
        // resolve to, and for the one an `auth_plugin_uuid` reference names: the
        // in-use scan reads these very rows, so the transition is the write
        // itself and needs no separate bookkeeping.
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-02

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-13
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-03
        // Publishing the snapshot is also what makes the stored `enabled`
        // boolean visible to the data plane.
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-03
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-13

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-09
        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-10
        // An invariant violation above left the staged change discarded —
        // nothing was inserted — and the tenant write lock released when the
        // guard dropped; the violation goes back to the calling flow for the
        // entry-2.1 mapping layer.
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-10
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-09

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-11
        Ok(record)
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-11
    }

    /// Replace an upstream, keeping `id`, `tenant_id` and `alias` immutable.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError`] when the replacement is not the stored
    /// identity, or when the plugin-position invariant is violated.
    pub fn replace_upstream(
        &self,
        existing: &Upstream,
        mut replacement: Upstream,
    ) -> Result<Arc<Upstream>, ManagementError> {
        let _guard = self.lock(existing.tenant_id);
        if replacement.tenant_id != existing.tenant_id || replacement.id != existing.id {
            return Err(ManagementError::Rejection(ManagementRejection::Identity(
                "id and tenant_id are immutable".to_string(),
            )));
        }
        if replacement.alias != existing.alias {
            return Err(ManagementError::Rejection(ManagementRejection::AliasConflict(
                "the alias is immutable".to_string(),
            )));
        }

        assert_bindings_consistent(replacement.plugins.as_ref().map(|plugins| &plugins.items))?;

        // `created_at` is refreshed on replace, because the list contract orders
        // on it.
        replacement.created_at = crate::domain::model::Timestamp::now();
        let record = Arc::new(replacement);
        let key = (record.tenant_id, record.id);
        self.upstreams.insert(key, Arc::clone(&record));
        self.alias_index
            .insert((record.tenant_id, record.alias.clone()), record.id);

        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-03
        // `in_use` -> `available` for a definition whose last referencing
        // binding row or `auth_plugin_uuid` column the replacement dropped: the
        // row set is the only record of the reference, so overwriting it is the
        // transition, and a later delete then finds no live reference.
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-03

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-11
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-11
        Ok(record)
    }

    /// Delete an upstream and cascade its routes.
    ///
    /// Returns the routes the cascade removed, so the caller can report the
    /// cascade in the response.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::NotFound`] when the upstream is unknown.
    pub fn delete_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Vec<Arc<Route>>, ManagementError> {
        let _guard = self.lock(tenant_id);
        let key = (tenant_id, id);
        let Some(record) = self.upstreams.remove(&key).map(|(_key, value)| value) else {
            return Err(ManagementError::not_found(format!(
                "no upstream `{id}` in this tenant"
            )));
        };

        let cascade = self.remove_routes_of_upstream(tenant_id, id);
        self.alias_index
            .remove(&(tenant_id, record.alias.clone()));

        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-08
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-08
        Ok(cascade)
    }

    /// Insert a route, enforcing the match-determinism invariant.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError`] when the referenced upstream is unknown in
    /// this tenant, the match rule collides with another enabled route, or the
    /// plugin-position invariant is violated.
    pub fn insert_route(&self, mut route: Route) -> Result<Arc<Route>, ManagementError> {
        let _guard = self.lock(route.tenant_id);
        if !self.upstreams.contains_key(&(route.tenant_id, route.upstream_id)) {
            return Err(ManagementError::Domain(DomainError::ValidationError {
                detail: format!("upstream_id: `{}` does not resolve in this tenant", route.upstream_id),
            }));
        }

        let siblings = self.list_routes_of_upstream(route.tenant_id, route.upstream_id);
        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-03
        // No two enabled routes of one upstream may share the same `path`,
        // `priority` and method combination.
        assert_match_determinism(&siblings, &route)?;
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-03
        assert_bindings_consistent(route.plugins.as_ref().map(|plugins| &plugins.items))?;

        route.created_at = crate::domain::model::Timestamp::now();
        let record = Arc::new(route);
        let key = (record.tenant_id, record.id);
        self.routes.insert(key, Arc::clone(&record));
        self.upstream_routes
            .entry((record.tenant_id, record.upstream_id))
            .or_default()
            .push(record.id);

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-11
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-11
        Ok(record)
    }

    /// Replace a route, keeping `id`, `tenant_id` and `upstream_id` immutable.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError`] when the replacement is not the stored
    /// identity, the match rule collides, or the plugin positions are not
    /// contiguous.
    pub fn replace_route(
        &self,
        existing: &Route,
        mut replacement: Route,
    ) -> Result<Arc<Route>, ManagementError> {
        let _guard = self.lock(existing.tenant_id);
        if replacement.tenant_id != existing.tenant_id || replacement.id != existing.id {
            return Err(ManagementError::Rejection(ManagementRejection::Identity(
                "id and tenant_id are immutable".to_string(),
            )));
        }
        if replacement.upstream_id != existing.upstream_id {
            return Err(ManagementError::Rejection(ManagementRejection::Identity(
                "upstream_id is immutable".to_string(),
            )));
        }

        let siblings = self
            .list_routes_of_upstream(existing.tenant_id, existing.upstream_id)
            .into_iter()
            .filter(|route| route.id != existing.id)
            .collect::<Vec<_>>();
        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-09
        assert_match_determinism(&siblings, &replacement)?;
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-09
        assert_bindings_consistent(replacement.plugins.as_ref().map(|plugins| &plugins.items))?;

        replacement.created_at = crate::domain::model::Timestamp::now();
        let record = Arc::new(replacement);
        self.routes
            .insert((record.tenant_id, record.id), Arc::clone(&record));

        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-11
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-11
        Ok(record)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::NotFound`] when the route is unknown.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), ManagementError> {
        let _guard = self.lock(tenant_id);
        let key = (tenant_id, id);
        let Some(record) = self.routes.remove(&key).map(|(_key, value)| value) else {
            return Err(ManagementError::not_found(format!(
                "no route `{id}` in this tenant"
            )));
        };
        if let Some(mut siblings) = self
            .upstream_routes
            .get_mut(&(tenant_id, record.upstream_id))
            .map(|entry| entry.value().clone())
        {
            siblings.retain(|route_id| *route_id != id);
            self.upstream_routes
                .insert((tenant_id, record.upstream_id), siblings);
        }

        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-08
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-08
        Ok(())
    }

    /// The upstreams of one tenant carrying one of `tags`.
    #[must_use]
    pub fn find_upstreams_by_tag(&self, tenant_id: Uuid, tag: &str) -> Vec<Arc<Upstream>> {
        self.list_upstreams(tenant_id)
            .into_iter()
            .filter(|upstream| upstream.tags.iter().any(|candidate| candidate == tag))
            .collect()
    }

    /// Take the write lock of one tenant key.
    fn lock(&self, tenant_id: Uuid) -> ArcMutexGuard<RawMutex, ()> {
        if !self.tenant_locks.contains_key(&tenant_id) {
            self.tenant_locks.insert(tenant_id, Arc::new(Mutex::new(())));
        }
        self.tenant_locks
            .get(&tenant_id)
            .map(|entry| Arc::clone(entry.value()))
            .unwrap_or_default()
            .lock_arc()
    }

    /// `UNIQUE (tenant_id, alias)`, checked inside the caller's critical section.
    fn assert_alias_free(&self, tenant_id: Uuid, alias: &str) -> Result<(), ManagementError> {
        match self.alias_index.get(&(tenant_id, alias.to_string())) {
            Some(_id) => Err(ManagementError::Rejection(
                ManagementRejection::AliasConflict(format!(
                    "an upstream with alias `{alias}` already exists in this tenant"
                )),
            )),
            None => Ok(()),
        }
    }

    /// Remove the routes of one upstream and return them.
    fn remove_routes_of_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        let removed = self.list_routes_of_upstream(tenant_id, upstream_id);
        for route in &removed {
            self.routes.remove(&(tenant_id, route.id));
        }
        self.upstream_routes.remove(&(tenant_id, upstream_id));
        removed
    }

    // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-01
    /// Insert a plugin definition, enforcing `UNIQUE (tenant_id, name)`.
    ///
    /// The definition's `id` and `tenant_id` are stamped here and never change
    /// afterwards, because the definition is immutable
    /// (`cpt-cf-oagw-dod-plugin-immutability`, `inst-full`).
    ///
    /// # Errors
    ///
    /// Returns a `409` when another definition of the same tenant already holds
    /// the `(tenant_id, name)` key.
    pub fn insert_plugin(&self, mut plugin: Plugin) -> Result<Arc<Plugin>, ManagementError> {
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-01
        let _guard = self.lock(plugin.tenant_id);

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-09
        // The `(tenant_id, name)` key is checked against the tenant's own
        // definitions, inside the same critical section as the write.
        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-02
        // `UNIQUE (tenant_id, name)`, checked inside the same critical section
        // as the write.
        let name_key = (plugin.tenant_id, plugin.name.clone());
        if self.plugin_name_index.contains_key(&name_key) {
            return Err(ManagementError::Rejection(
                ManagementRejection::AliasConflict(format!(
                    "a plugin named `{}` already exists in this tenant",
                    plugin.name
                )),
            ));
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-02
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-09

        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-05
        // The definition is immutable: the identity is stamped on insert and no
        // replace operation exists.
        crate::domain::plugin::definition_uuid(&plugin.id).ok_or_else(|| {
            ManagementError::Domain(DomainError::ValidationError {
                detail: format!("id: `{}` is not a plugin definition identifier", plugin.id),
            })
        })?;
        plugin.last_used_at = None;
        plugin.gc_eligible_at = None;
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-05

        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-09
        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-10
        // A rejection above returns before this point: the staged change is
        // discarded, the guard releases the tenant lock on scope exit and the
        // violation goes back to the calling flow for the entry-2.1 mapping
        // layer, with no partial write behind.
        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-04
        let record = Arc::new(plugin);
        self.plugin_name_index
            .insert(name_key, record.id.clone());
        self.plugins
            .insert((record.tenant_id, record.id.clone()), Arc::clone(&record));
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-04
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-10
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-09

        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-06
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-11
        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-07
        self.publish();
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-07
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-11
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-06

        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-11
        Ok(record)
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-11
    }

    /// Delete a plugin definition after the in-use scan.
    ///
    /// The scan and the delete share the tenant's write lock, so no binding can
    /// start referencing the definition between the check and the write
    /// (`cpt-cf-oagw-algo-plugin-in-use-scan`, `inst-pinu-01`).
    ///
    /// # Errors
    ///
    /// Returns [`ManagementError::not_found`] when the definition is unknown and
    /// a `409` carrying `plugin_id` and `referenced_by` when a live reference
    /// remains.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> Result<(), ManagementError> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-01
        // The store's write lock for the affected tenant key: the scan and the
        // delete observe the same snapshot.
        let _guard = self.lock(tenant_id);
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-01
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-08
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-07
        let Some(record) = self
            .plugins
            .get(&(tenant_id, id.to_string()))
            .map(|entry| Arc::clone(entry.value()))
        else {
            return Err(ManagementError::not_found(format!(
                "no plugin `{id}` in this tenant"
            )));
        };
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-07

        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-09
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-08
        let references = self.scan_plugin_references(&record);
        if !references.is_empty() {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-09
            // The staged delete is discarded: nothing was removed, so the store
            // is untouched and the reference list goes back to the calling flow,
            // which maps it to the `409` with `plugin_id` and `referenced_by`.
            // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-10
            // The staged delete is discarded and the reference list goes back to
            // the calling flow, which maps it to the `409` with `plugin_id` and
            // `referenced_by`; nothing else leaves this function.
            return Err(ManagementError::plugin_in_use(record.id.clone(), references));
            // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-10
            // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-09
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-08
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-09
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-08

        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-10
        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-05
        // `in_use` -> `absent` when the scan found no live reference any more;
        // the transition is observed by the delete that returns `204`.
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-11
        // No garbage collection and no `gc_eligible_at` writer: the GC job of
        // the DESIGN lifecycle is out of scope in this deployment.
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-11

        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-11
        self.plugins.remove(&(tenant_id, id.to_string()));
        // The name index holds the same key the uniqueness check wrote, so the
        // delete removes exactly one index entry.
        self.plugin_name_index.remove(&(tenant_id, record.name.clone()));
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-11

        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-12
        self.publish();
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-12
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-05
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-10
        Ok(())
    }

    /// The definition of one tenant carrying `id`, or `None`.
    #[must_use]
    pub fn find_plugin(&self, tenant_id: Uuid, id: &str) -> Option<Arc<Plugin>> {
        self.plugins
            .get(&(tenant_id, id.to_string()))
            .map(|entry| Arc::clone(entry.value()))
    }

    /// The definition of one tenant whose identifier ends with the UUID
    /// `instance`, or `None`.
    ///
    /// Used by the binding validator, which resolves a UUID-backed reference
    /// through the stored row (`cpt-cf-oagw-algo-plugin-binding-validate`,
    /// `inst-pbnd-04`).
    #[must_use]
    pub fn find_plugin_by_uuid(&self, tenant_id: Uuid, instance: Uuid) -> Option<Arc<Plugin>> {
        self.list_plugins(tenant_id)
            .into_iter()
            .find(|plugin| plugin.uuid() == Some(instance))
    }

    /// Every definition of one tenant.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>> {
        self.plugins
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Every live reference to one definition, inside the caller's critical
    /// section.
    ///
    /// The scan reads the stored reference columns only
    /// (`inst-pinu-05`): the binding rows of `oagw_upstream_plugin` and
    /// `oagw_route_plugin` and the upstream `auth_plugin_ref` /
    /// `auth_plugin_uuid` columns. A reference matches on its identifier or on
    /// its UUID instance, never on a string that merely resembles it, and the
    /// result carries the referencing resource, its identifier and the binding
    /// position — and nothing else (`inst-pinu-04`).
    fn scan_plugin_references(&self, plugin: &Plugin) -> Vec<String> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-12
        let instance = plugin.uuid();
        let mut references = Vec::new();
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-06
        // The scan walks only the rows of the definition's own tenant: binding
        // validation resolves a reference inside the caller's tenant key space,
        // so no foreign reference exists to find.
        for entry in self.upstreams.iter() {
            let upstream = entry.value();
            if upstream.tenant_id != plugin.tenant_id {
                continue;
            }
            // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-02
            // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-04
            // The `oagw_upstream_plugin` binding rows, matched on the stored
            // reference columns: the result names the referencing resource, its
            // identifier and the binding position, and nothing else.
            for binding in bindings_of(&upstream.plugins) {
                if binding_matches(binding, plugin, instance) {
                    references.push(format!(
                        "upstreams/{}/plugins/{}",
                        upstream.id, binding.position
                    ));
                }
            }
            // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-04
            // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-02
            // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-03
            // The upstream `auth_plugin_ref` / `auth_plugin_uuid` columns: the
            // auth plugin identity is stored outside the binding rows precisely
            // so this check stays typed and never depends on JSON scanning.
            if auth_plugin_matches(upstream, plugin, instance) {
                references.push(format!("upstreams/{}/auth", upstream.id));
            }
            // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-03
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-06
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-02
        // The `oagw_route_plugin` binding rows, named with the route identifier
        // and the binding position.
        for entry in self.routes.iter() {
            let route = entry.value();
            if route.tenant_id != plugin.tenant_id {
                continue;
            }
            for binding in bindings_of(&route.plugins) {
                if binding_matches(binding, plugin, instance) {
                    references
                        .push(format!("routes/{}/plugins/{}", route.id, binding.position));
                }
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-02
        // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-05
        // The match is on the stored reference columns only — an identifier
        // equality or a UUID instance equality — so a definition is never
        // reported as used because of a string that merely resembles its
        // identifier.
        references.sort();
        references.dedup();
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-05
        references
        // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-scan:p1:inst-pinu-12
    }

    /// Publish a new immutable snapshot and invalidate the consumer caches.
    ///
    /// This is the tail of `cpt-cf-oagw-algo-store-invariants` shared by every
    /// write: steps 7 and 8 of the algorithm, which run only after a write that
    /// was not caught by an invariant.
    fn publish(&self) {
        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-07
        let mut upstreams = BTreeMap::new();
        for entry in self.upstreams.iter() {
            upstreams.insert(*entry.key(), Arc::clone(entry.value()));
        }
        let mut routes = BTreeMap::new();
        for entry in self.routes.iter() {
            routes.insert(*entry.key(), Arc::clone(entry.value()));
        }
        let mut plugins = BTreeMap::new();
        for entry in self.plugins.iter() {
            plugins.insert(entry.key().clone(), Arc::clone(entry.value()));
        }
        let epoch = self.snapshot.load().epoch + 1;
        self.snapshot.store(Arc::new(ConfigSnapshot {
            epoch,
            upstreams,
            routes,
            plugins,
        }));
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-07

        // @cpt-begin:cpt-cf-oagw-algo-store-invariants:p1:inst-store-08
        // `cpt-cf-oagw-adr-state-management`: the data plane resolves from the
        // snapshot, so a bumped epoch is the cache invalidation signal.
        self.invalidations.fetch_add(1, Ordering::Release);
        // @cpt-end:cpt-cf-oagw-algo-store-invariants:p1:inst-store-08
    }
}

// @cpt-begin:cpt-cf-oagw-dod-store-invariants:p1:inst-full
/// The store as the [`UpstreamRepository`] of `cpt-cf-oagw-dod-store-invariants`.
///
/// Delegation only: the invariant-bearing bodies live on [`OagwStore`], and the
/// trait is the contract the domain service is written against.
impl crate::domain::repo::UpstreamRepository for OagwStore {
    fn insert_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, ManagementError> {
        OagwStore::insert_upstream(self, upstream)
    }

    fn replace_upstream(
        &self,
        existing: &Upstream,
        replacement: Upstream,
    ) -> Result<Arc<Upstream>, ManagementError> {
        OagwStore::replace_upstream(self, existing, replacement)
    }

    fn delete_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Vec<Arc<Route>>, ManagementError> {
        OagwStore::delete_upstream(self, tenant_id, id)
    }

    fn find_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        OagwStore::find_upstream(self, tenant_id, id)
    }

    fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        OagwStore::find_upstream_by_alias(self, tenant_id, alias)
    }

    fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        OagwStore::list_upstreams(self, tenant_id)
    }
}

/// The store as the [`RouteRepository`] of `cpt-cf-oagw-dod-store-invariants`.
impl crate::domain::repo::RouteRepository for OagwStore {
    fn insert_route(&self, route: Route) -> Result<Arc<Route>, ManagementError> {
        OagwStore::insert_route(self, route)
    }

    fn replace_route(
        &self,
        existing: &Route,
        replacement: Route,
    ) -> Result<Arc<Route>, ManagementError> {
        OagwStore::replace_route(self, existing, replacement)
    }

    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), ManagementError> {
        OagwStore::delete_route(self, tenant_id, id)
    }

    fn find_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>> {
        OagwStore::find_route(self, tenant_id, id)
    }

    fn list_routes(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        OagwStore::list_routes(self, tenant_id)
    }

    fn list_routes_of_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        OagwStore::list_routes_of_upstream(self, tenant_id, upstream_id)
    }
}
// @cpt-end:cpt-cf-oagw-dod-store-invariants:p1:inst-full

/// The store as the [`PluginRepository`] of
/// `cpt-cf-oagw-dod-plugin-store-invariants`.
///
/// Delegation only: the invariant-bearing bodies live on [`OagwStore`], and the
/// trait is the contract the domain service is written against.
impl crate::domain::repo::PluginRepository for OagwStore {
    fn insert_plugin(&self, plugin: Plugin) -> Result<Arc<Plugin>, ManagementError> {
        OagwStore::insert_plugin(self, plugin)
    }

    fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> Result<(), ManagementError> {
        OagwStore::delete_plugin(self, tenant_id, id)
    }

    fn find_plugin(&self, tenant_id: Uuid, id: &str) -> Option<Arc<Plugin>> {
        OagwStore::find_plugin(self, tenant_id, id)
    }

    fn find_plugin_by_uuid(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Plugin>> {
        OagwStore::find_plugin_by_uuid(self, tenant_id, id)
    }

    fn list_plugins(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>> {
        OagwStore::list_plugins(self, tenant_id)
    }
}

/// The `plugins.items[]` rows of a record, empty when no chain is declared.
fn bindings_of(plugins: &Option<crate::domain::model::PluginsConfig>) -> &[crate::domain::model::PluginBinding] {
    match plugins.as_ref() {
        Some(plugins) => &plugins.items,
        None => &[],
    }
}

/// Whether one binding row references the definition.
///
/// The match is on the stored reference columns only
/// (`cpt-cf-oagw-algo-plugin-in-use-scan`, `inst-pinu-05`): the UUID column when
/// the definition is UUID-backed, and the exact identifier of the definition —
/// never a string that merely resembles it.
fn binding_matches(
    binding: &crate::domain::model::PluginBinding,
    plugin: &Plugin,
    instance: Option<Uuid>,
) -> bool {
    binding.plugin_uuid == instance
        || instance.is_some() && binding.reference == plugin.id
}

/// Whether an upstream's scalar auth columns reference the definition.
fn auth_plugin_matches(upstream: &Upstream, plugin: &Plugin, instance: Option<Uuid>) -> bool {
    if let Some(uuid) = upstream.auth_plugin_uuid {
        return Some(uuid) == instance;
    }
    instance.is_some() && upstream.auth_plugin_ref.as_deref() == Some(plugin.id.as_str())
}

/// The binding-row invariants of a written plugin row set
/// (`cpt-cf-oagw-algo-plugin-store-write`, `inst-pstr-03`).
///
/// Positions are contiguous from `0`, `plugin_ref` is always stored, and
/// `plugin_uuid` is stored only for a UUID-backed plugin, matching the instance
/// part of its reference when both are present.
fn assert_bindings_consistent(
    items: Option<&Vec<crate::domain::model::PluginBinding>>,
) -> Result<(), ManagementError> {
    assert_positions_contiguous(items)?;
    let Some(items) = items else {
        return Ok(());
    };
    for item in items {
        if item.reference.is_empty() {
            return Err(ManagementError::Domain(DomainError::ValidationError {
                detail: format!(
                    "plugins.items[{}].plugin_ref: required",
                    item.position
                ),
            }));
        }
        if item.plugin_uuid != crate::domain::validation::uuid_of_plugin_reference(&item.reference)
        {
            return Err(ManagementError::Domain(DomainError::ValidationError {
                detail: format!(
                    "plugins.items[{}].plugin_uuid: does not match plugin_ref",
                    item.position
                ),
            }));
        }
    }
    Ok(())
}

/// Whether a written plugin row set is contiguous from `0`.
fn assert_positions_contiguous(
    items: Option<&Vec<crate::domain::model::PluginBinding>>,
) -> Result<(), ManagementError> {
    let Some(items) = items else {
        return Ok(());
    };
    for (expected, item) in items.iter().enumerate() {
        if usize::try_from(item.position) != Ok(expected) {
            return Err(ManagementError::Domain(DomainError::ValidationError {
                detail: format!(
                    "plugins.items[{}].position: positions must be contiguous from 0",
                    item.position
                ),
            }));
        }
    }
    Ok(())
}

/// Route match determinism: no two *enabled* routes of one upstream may share
/// the same `path`, `priority` and method combination.
fn assert_match_determinism(siblings: &[Arc<Route>], candidate: &Route) -> Result<(), ManagementError> {
    if !candidate.enabled {
        return Ok(());
    }
    let Some(matched) = candidate.matches.http.as_ref() else {
        return Ok(());
    };
    for sibling in siblings {
        if !sibling.enabled || sibling.id == candidate.id {
            continue;
        }
        let Some(HttpMatch {
            path: sibling_path,
            methods: sibling_methods,
            ..
        }) = sibling.matches.http.as_ref()
        else {
            continue;
        };
        let same_key = sibling.priority == candidate.priority && sibling_path == &matched.path;
        let shares_method = matched
            .methods
            .iter()
            .any(|method| sibling_methods.contains(method));
        if same_key && shares_method {
            return Err(ManagementError::Rejection(
                ManagementRejection::RouteMatchConflict(format!(
                    "route `{}` already matches `{}` with priority {} and method `{}`",
                    sibling.id, matched.path, candidate.priority, matched.methods[0]
                )),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, MatchRule, MatchType, Protocol, Scheme, ServerConfig, Timestamp,
    };
    use crate::domain::validation::validate_upstream;

    const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000000aa");
    const OTHER: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000000bb");

    fn upstream(id: Uuid, alias: &str) -> Upstream {
        Upstream {
            id,
            tenant_id: TENANT,
            enabled: true,
            alias: alias.to_string(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.vendor.com".to_string(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: Timestamp::from_nanos(0),
        }
    }

    fn route(id: Uuid, upstream_id: Uuid, path: &str, priority: i32, enabled: bool) -> Route {
        Route {
            id,
            tenant_id: TENANT,
            upstream_id,
            enabled,
            matches: MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_string()],
                    path: path.to_string(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::SuffixMode::Append,
                }),
                grpc: None,
            },
            match_type: MatchType::Http,
            priority,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: Timestamp::from_nanos(0),
        }
    }

    #[test]
    fn an_insert_stamps_created_at_and_publishes_a_snapshot() {
        let store = OagwStore::new();
        let id = Uuid::new_v4();
        let record = store.insert_upstream(upstream(id, "api.vendor.com")).expect("inserted");
        assert!(record.created_at.as_nanos() > 0);
        assert_eq!(store.snapshot_epoch(), 1);
        assert_eq!(store.invalidation_count(), 1);
        assert!(store.find_upstream(TENANT, id).is_some());
    }

    #[test]
    fn the_alias_key_is_unique_per_tenant() {
        let store = OagwStore::new();
        store
            .insert_upstream(upstream(Uuid::new_v4(), "api.vendor.com"))
            .expect("first insert");
        let error = store
            .insert_upstream(upstream(Uuid::new_v4(), "api.vendor.com"))
            .expect_err("same tenant, same alias");
        assert_eq!(error.status(), 409);

        // A different tenant owns its own key space.
        let mut foreign = upstream(Uuid::new_v4(), "api.vendor.com");
        foreign.tenant_id = OTHER;
        store.insert_upstream(foreign).expect("other tenant");
        assert_eq!(store.list_upstreams(OTHER).len(), 1);
    }

    #[test]
    fn the_alias_index_resolves_by_alias() {
        let store = OagwStore::new();
        let id = Uuid::new_v4();
        store
            .insert_upstream(upstream(id, "api.vendor.com"))
            .expect("inserted");
        assert_eq!(
            store
                .find_upstream_by_alias(TENANT, "api.vendor.com")
                .map(|upstream| upstream.id),
            Some(id)
        );
        assert!(store.find_upstream_by_alias(OTHER, "api.vendor.com").is_none());
    }

    #[test]
    fn a_replace_keeps_the_identity_and_refreshes_created_at() {
        let store = OagwStore::new();
        let id = Uuid::new_v4();
        store.insert_upstream(upstream(id, "api.vendor.com")).expect("inserted");
        let existing = store.find_upstream(TENANT, id).expect("stored");
        let mut replacement = (*existing).clone();
        replacement.tags = vec!["core".to_string()];
        let replaced = store.replace_upstream(&existing, replacement).expect("replaced");
        assert_eq!(replaced.tags, vec!["core".to_string()]);
        assert!(replaced.created_at >= existing.created_at);
        assert_eq!(replaced.alias, "api.vendor.com");

        let mut renamed = (*existing).clone();
        renamed.alias = "other.example.com".to_string();
        let error = store
            .replace_upstream(&existing, renamed)
            .expect_err("the alias is immutable");
        assert_eq!(error.status(), 409);
    }

    #[test]
    fn deleting_an_upstream_cascades_its_routes() {
        let store = OagwStore::new();
        let id = Uuid::new_v4();
        store.insert_upstream(upstream(id, "api.vendor.com")).expect("inserted");
        store
            .insert_route(route(Uuid::new_v4(), id, "/v1/pay", 0, true))
            .expect("route inserted");
        store
            .insert_route(route(Uuid::new_v4(), id, "/v1/refund", 0, true))
            .expect("route inserted");

        let cascade = store.delete_upstream(TENANT, id).expect("deleted");
        assert_eq!(cascade.len(), 2, "both routes are cascaded");
        assert!(store.find_upstream(TENANT, id).is_none());
        assert!(store.list_routes(TENANT).is_empty());
        assert!(store.find_upstream_by_alias(TENANT, "api.vendor.com").is_none());
    }

    #[test]
    fn deleting_an_unknown_upstream_is_a_not_found() {
        let store = OagwStore::new();
        let error = store.delete_upstream(TENANT, Uuid::new_v4()).expect_err("unknown");
        assert_eq!(error.status(), 404);
    }

    #[test]
    fn a_route_requires_a_resolvable_upstream_of_the_same_tenant() {
        let store = OagwStore::new();
        let mut foreign = upstream(Uuid::new_v4(), "api.vendor.com");
        foreign.tenant_id = OTHER;
        let foreign_id = foreign.id;
        store.insert_upstream(foreign).expect("inserted");

        let error = store
            .insert_route(route(Uuid::new_v4(), foreign_id, "/v1", 0, true))
            .expect_err("another tenant's upstream is not resolvable");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn two_enabled_routes_may_not_share_path_priority_and_method() {
        let store = OagwStore::new();
        let upstream_id = Uuid::new_v4();
        store.insert_upstream(upstream(upstream_id, "api.vendor.com")).expect("inserted");
        store
            .insert_route(route(Uuid::new_v4(), upstream_id, "/v1/pay", 10, true))
            .expect("first route");

        let error = store
            .insert_route(route(Uuid::new_v4(), upstream_id, "/v1/pay", 10, true))
            .expect_err("same path, priority and method");
        assert_eq!(error.status(), 409);

        // A different priority or path does not collide.
        store
            .insert_route(route(Uuid::new_v4(), upstream_id, "/v1/pay", 11, true))
            .expect("different priority");
        store
            .insert_route(route(Uuid::new_v4(), upstream_id, "/v1/pay/", 10, true))
            .expect("different path");

        // A disabled route does not participate in the invariant.
        store
            .insert_route(route(Uuid::new_v4(), upstream_id, "/v1/pay", 10, false))
            .expect("disabled route");
    }

    #[test]
    fn a_disabled_route_may_collide_until_it_is_re_enabled() {
        let store = OagwStore::new();
        let upstream_id = Uuid::new_v4();
        store.insert_upstream(upstream(upstream_id, "api.vendor.com")).expect("inserted");
        let disabled_id = Uuid::new_v4();
        store
            .insert_route(route(disabled_id, upstream_id, "/v1", 0, false))
            .expect("disabled route");
        store
            .insert_route(route(Uuid::new_v4(), upstream_id, "/v1", 0, true))
            .expect("no conflict with a disabled route");

        let existing = store.find_route(TENANT, disabled_id).expect("stored");
        let mut replacement = (*existing).clone();
        replacement.enabled = true;
        let error = store
            .replace_route(&existing, replacement)
            .expect_err("re-enabling collides");
        assert_eq!(error.status(), 409);
    }

    #[test]
    fn a_route_replacement_keeps_upstream_id_immutable() {
        let store = OagwStore::new();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        store.insert_upstream(upstream(first, "api.vendor.com")).expect("inserted");
        store.insert_upstream(upstream(second, "other.example.com")).expect("inserted");
        let route_id = Uuid::new_v4();
        store
            .insert_route(route(route_id, first, "/v1", 0, true))
            .expect("inserted");

        let existing = store.find_route(TENANT, route_id).expect("stored");
        let mut replacement = (*existing).clone();
        replacement.upstream_id = second;
        let error = store
            .replace_route(&existing, replacement)
            .expect_err("the reference is immutable");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn deleting_a_route_removes_only_that_route() {
        let store = OagwStore::new();
        let upstream_id = Uuid::new_v4();
        store.insert_upstream(upstream(upstream_id, "api.vendor.com")).expect("inserted");
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        store.insert_route(route(first, upstream_id, "/a", 0, true)).expect("inserted");
        store.insert_route(route(second, upstream_id, "/b", 0, true)).expect("inserted");

        store.delete_route(TENANT, first).expect("deleted");
        assert_eq!(store.list_routes_of_upstream(TENANT, upstream_id).len(), 1);
        assert_eq!(store.list_routes_of_upstream(TENANT, upstream_id)[0].id, second);
    }

    #[test]
    fn the_snapshot_is_untouched_by_a_rejected_write() {
        let store = OagwStore::new();
        store.insert_upstream(upstream(Uuid::new_v4(), "api.vendor.com")).expect("inserted");
        let before = store.snapshot_epoch();
        let routes_before = store.snapshot().routes.len();

        let error = store
            .insert_upstream(upstream(Uuid::new_v4(), "api.vendor.com"))
            .expect_err("alias conflict");
        assert_eq!(error.status(), 409);
        assert_eq!(store.snapshot_epoch(), before, "no snapshot published");
        assert_eq!(store.snapshot().routes.len(), routes_before);
        assert_eq!(store.snapshot().upstreams.len(), 1, "no partial write");
    }

    #[test]
    fn plugin_positions_must_be_contiguous_from_zero() {
        let store = OagwStore::new();
        let mut record = upstream(Uuid::new_v4(), "api.vendor.com");
        record.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::Sharing::Private,
            items: vec![crate::domain::model::PluginBinding {
                position: 1,
                reference: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
                    .to_string(),
                plugin_uuid: None,
                config: None,
            }],
        });
        let error = store.insert_upstream(record).expect_err("gap at position 0");
        assert!(error.detail().contains("contiguous"), "{}", error.detail());
    }

    #[test]
    fn listing_is_scoped_to_the_calling_tenant() {
        let store = OagwStore::new();
        store.insert_upstream(upstream(Uuid::new_v4(), "one.example.com")).expect("inserted");
        let mut foreign = upstream(Uuid::new_v4(), "two.example.com");
        foreign.tenant_id = OTHER;
        store.insert_upstream(foreign).expect("inserted");

        assert_eq!(store.list_upstreams(TENANT).len(), 1);
        assert_eq!(store.list_upstreams(OTHER).len(), 1);
        assert_eq!(store.find_upstreams_by_tag(TENANT, "core").len(), 0);
    }

    #[test]
    fn a_stored_upstream_round_trips_through_the_validator() {
        let store = OagwStore::new();
        let body = serde_json::json!({
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": crate::domain::model::PROTOCOL_HTTP,
            "rate_limit": { "sustained": { "rate": 10 } }
        });
        let spec: crate::domain::validation::UpstreamSpec =
            serde_json::from_value(body).expect("bindable");
        let validated = validate_upstream(&spec, TENANT, Timestamp::from_nanos(0)).expect("valid");
        let id = validated.id;
        store.insert_upstream(validated).expect("inserted");
        let stored = store.find_upstream(TENANT, id).expect("stored");
        assert_eq!(stored.rate_limit.as_ref().expect("rate limit").sustained.rate, 10);
    }
}
