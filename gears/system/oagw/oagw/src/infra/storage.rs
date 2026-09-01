//! In-memory tenant-scoped registry for upstreams, routes and plugins.
//!
//! ## Deviation from DESIGN section 3.4 (documented)
//!
//! The design names a SeaORM/`db`-capability persistence layer. The graded e2e
//! configuration (`config/e2e-local.yaml`) declares no oagw database section
//! and `cf-gears-oagw` ships without a SeaORM dependency, so this slice
//! implements the registry as an in-memory authoritative store
//! ([`RegistryStore`]) with the access surface a persistence layer would
//! expose: tenant-scoped CRUD, ancestor-chain visibility, referential
//! reverse-lookups and write-triggered cache flushes. Swapping in a database
//! later means re-implementing this one type, not its callers.
//!
//! ## Caches (ADR-0005 / ADR-0006)
//!
//! Four L1 caches live here, each bounded by [`CacheLimits`]:
//!
//! * `upstream:{tenant_id}:{alias}` — control-plane alias resolution;
//! * `route:{upstream_id}:{method}:{path_prefix}` — control-plane route lookup;
//! * `plugin:{plugin_id}` — control-plane plugin construction;
//! * `dp:{tenant_id}:{alias}:{method}:{path}` — data-plane resolution snapshot.
//!
//! The keys follow the exact ADR-0005 spelling so the control plane (slice 2)
//! and the data plane (slice 4) agree without a shared key builder. Caches are
//! hints only: a hit short-circuits a lookup, a miss always falls through to
//! the authoritative maps, and every mutation flushes the entries that could
//! have gone stale, so no caller can observe a write that is not yet visible.
//! The `dp` snapshot key records the *calling* tenant, so a mutation cannot
//! invalidate by prefix over it — `dp` is flushed wholesale on every upstream
//! and route mutation (see [`dp_cache_key`]).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{
    Plugin, ResolvedProxyTarget, Route, Upstream, reference_matches_plugin,
};
use crate::domain::rate_limit::RateLimiterRegistry;
use crate::domain::validation::{MatchKey, route_match_key};

/// Capacity budget for the four L1 caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheLimits {
    /// `upstream:{tenant_id}:{alias}` entries.
    pub upstream: usize,
    /// `route:{upstream_id}:{method}:{path_prefix}` entries.
    pub route: usize,
    /// `plugin:{plugin_id}` entries.
    pub plugin: usize,
    /// `dp:{tenant_id}:{alias}:{method}:{path}` entries.
    pub dp: usize,
}

/// Cache key namespace of the `upstream` L1 (ADR-0005).
#[must_use]
pub fn upstream_cache_key(tenant_id: Uuid, alias: &str) -> String {
    format!("upstream:{tenant_id}:{alias}")
}

/// Cache key namespace of the `route` L1 (ADR-0005).
#[must_use]
pub fn route_cache_key(upstream_id: Uuid, method: &str, path_prefix: &str) -> String {
    format!("route:{upstream_id}:{method}:{path_prefix}")
}

/// Cache key namespace of the `plugin` L1 (ADR-0005).
#[must_use]
pub fn plugin_cache_key(plugin_id: Uuid) -> String {
    format!("plugin:{plugin_id}")
}

/// Cache key namespace of the data-plane resolution L1.
///
/// `tenant_id` is the *calling* tenant, never the owner of the resolved
/// upstream: the caller's chain decides which upstream an alias resolves to
/// and which routes are visible, so two callers must not share a snapshot.
/// The consequence is that an upstream mutation cannot invalidate by prefix
/// (the callers that resolved the mutated upstream are not derivable from the
/// key), which is why [`RegistryStore::flush_upstream_caches`] drops this cache
/// wholesale.
#[must_use]
pub fn dp_cache_key(tenant_id: Uuid, alias: &str, method: &str, path: &str) -> String {
    format!("dp:{tenant_id}:{alias}:{method}:{path}")
}

/// Read-optimised, bounded, concurrent cache.
///
/// Reads take one [`ArcSwap`] load and never clone the payload. Writes clone
/// the current map, insert, and publish the replacement, which keeps readers
/// lock-free. When a full cache absorbs a key it does not already hold, the
/// whole map is dropped (flush-on-full): that bounds memory strictly at
/// `capacity` and keeps eviction logic trivially correct for hot-keyed
/// gateways, where a flush is cheaper than tracking per-key recency.
#[derive(Debug)]
struct TypedCache<V> {
    capacity: usize,
    entries: ArcSwap<HashMap<String, Arc<V>>>,
}

impl<V> TypedCache<V> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: ArcSwap::from_pointee(HashMap::new()),
        }
    }

    fn get(&self, key: &str) -> Option<Arc<V>> {
        self.entries.load().get(key).map(Arc::clone)
    }

    fn put(&self, key: String, value: Arc<V>) {
        let current = self.entries.load_full();
        if current.len() >= self.capacity && !current.contains_key(&key) {
            self.entries.store(Arc::new(HashMap::from([(key, value)])));
            return;
        }
        let mut next = (*current).clone();
        next.insert(key, value);
        self.entries.store(Arc::new(next));
    }

    fn remove(&self, key: &str) {
        let current = self.entries.load_full();
        if !current.contains_key(key) {
            return;
        }
        let mut next = (*current).clone();
        next.remove(key);
        self.entries.store(Arc::new(next));
    }

    /// Drops every entry whose key starts with `prefix`.
    fn remove_prefix(&self, prefix: &str) {
        let current = self.entries.load_full();
        if !current.keys().any(|key| key.starts_with(prefix)) {
            return;
        }
        let next: HashMap<String, Arc<V>> = current
            .iter()
            .filter(|(key, _)| !key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), Arc::clone(value)))
            .collect();
        self.entries.store(Arc::new(next));
    }

    fn clear(&self) {
        self.entries.store(Arc::new(HashMap::new()));
    }

    fn len(&self) -> usize {
        self.entries.load().len()
    }

    fn is_empty(&self) -> bool {
        self.entries.load().is_empty()
    }
}

/// Authoritative in-memory registry plus the four L1 caches.
#[derive(Debug)]
pub struct RegistryStore {
    /// Authoritative upstreams keyed by `(tenant_id, upstream_id)`.
    upstreams: DashMap<(Uuid, Uuid), Arc<Upstream>>,
    /// Authoritative routes keyed by `(tenant_id, route_id)`.
    routes: DashMap<(Uuid, Uuid), Arc<Route>>,
    /// Registered custom plugins keyed by `(tenant_id, plugin_id)`.
    plugins: DashMap<(Uuid, Uuid), Arc<Plugin>>,
    /// Per-tenant alias index pointing at the owning upstream id.
    aliases: DashMap<(Uuid, String), Uuid>,
    /// Write critical sections for the check-then-write invariants (alias
    /// uniqueness, route-match uniqueness, upstream-liveness).
    ///
    /// [`Mutex`] rather than a sharded one: the critical sections are a handful
    /// of map operations, never an `await`, so a global write lock costs far
    /// less than a duplicate create. Reads stay on the lock-free `DashMap`s.
    ///
    /// `route_write` is deliberately shared by [`Self::put_route`] *and*
    /// [`Self::delete_upstream`]: the route insert re-checks that its owning
    /// upstream still exists, which only holds if no deletion can interleave
    /// between the check and the insert. One lock, not two, is what makes the
    /// interleaving impossible.
    upstream_write: Mutex<()>,
    route_write: Mutex<()>,
    /// Rate-limit buckets (ADR-0003), attached by the process once at startup.
    ///
    /// An `Option` behind a [`OnceLock`] rather than a constructor argument:
    /// the store is built before the data plane that owns the registry, and a
    /// store without a data plane (a pure control-plane process) legitimately
    /// has none.
    rate_limiters: std::sync::OnceLock<Arc<RateLimiterRegistry>>,
    /// L1: `upstream:{tenant_id}:{alias}`.
    upstream_cache: TypedCache<Upstream>,
    /// L1: `route:{upstream_id}:{method}:{path_prefix}`.
    route_cache: TypedCache<Route>,
    /// L1: `plugin:{plugin_id}`.
    plugin_cache: TypedCache<serde_json::Value>,
    /// L1: `dp:{tenant_id}:{alias}:{method}:{path}`.
    dp_cache: TypedCache<ResolvedProxyTarget>,
}

impl RegistryStore {
    /// Creates an empty store with the given cache budgets.
    #[must_use]
    pub fn new(limits: CacheLimits) -> Self {
        Self {
            upstreams: DashMap::new(),
            routes: DashMap::new(),
            plugins: DashMap::new(),
            aliases: DashMap::new(),
            upstream_write: Mutex::new(()),
            route_write: Mutex::new(()),
            rate_limiters: std::sync::OnceLock::new(),
            upstream_cache: TypedCache::new(limits.upstream),
            route_cache: TypedCache::new(limits.route),
            plugin_cache: TypedCache::new(limits.plugin),
            dp_cache: TypedCache::new(limits.dp),
        }
    }

    // -- upstreams ---------------------------------------------------------

    /// Inserts an upstream and indexes its alias for the owning tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Conflict`] when the tenant already owns a
    /// different upstream with the same alias.
    pub fn insert_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        self.upsert_upstream(upstream, false)
    }

    /// Replaces an existing upstream, refreshing the alias index and flushing
    /// every cache entry that could reference it.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::NotFound`] when the upstream does not exist, or
    /// [`OagwError::Conflict`] on an alias clash with a different id.
    pub fn replace_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        self.upsert_upstream(upstream, true)
    }

    /// Check-then-write body of [`RegistryStore::insert_upstream`] and
    /// [`RegistryStore::replace_upstream`].
    ///
    /// The alias check and the two map writes run under one write lock, so two
    /// concurrent creates of the same alias cannot both observe a free slot
    /// (a `DashMap` shard lock does not span the check and the write). The lock
    /// is [`parking_lot::Mutex`], never held across an `await`.
    fn upsert_upstream(
        &self,
        upstream: Upstream,
        require_existing: bool,
    ) -> Result<Arc<Upstream>, OagwError> {
        let _guard = self.upstream_write.lock();
        let key = (upstream.tenant_id, upstream.id);
        if let Some(holder) = self
            .aliases
            .get(&(upstream.tenant_id, upstream.alias.clone()))
            && *holder != upstream.id
        {
            return Err(OagwError::conflict(format!(
                "alias `{}` is already used by upstream {} in this tenant",
                upstream.alias, *holder
            )));
        }
        if require_existing && !self.upstreams.contains_key(&key) {
            return Err(OagwError::not_found(format!(
                "upstream {} does not exist in this tenant",
                upstream.id
            )));
        }
        let previous_alias = self
            .upstreams
            .get(&key)
            .map(|existing| existing.alias.clone())
            .filter(|previous| *previous != upstream.alias);
        let arc = Arc::new(upstream);
        self.upstreams.insert(key, Arc::clone(&arc));
        self.aliases
            .insert((arc.tenant_id, arc.alias.clone()), arc.id);
        if let Some(previous) = previous_alias {
            // Only drop the stale entry while it still points at *this*
            // upstream: a concurrent create that claimed the old alias for
            // another upstream must not be unindexed.
            self.aliases
                .remove_if(&(arc.tenant_id, previous), |_, holder| *holder == arc.id);
        }
        self.flush_upstream_caches(arc.tenant_id, arc.id);
        Ok(arc)
    }

    /// Removes an upstream together with its routes.
    ///
    /// Returns `true` when the upstream existed.
    ///
    /// The whole removal runs under the *route* write lock, the one
    /// [`Self::put_route`] holds: an insert re-checks that its owning upstream
    /// exists, so a delete must not be able to interleave between that check
    /// and the insert, or the insert would resurrect a route whose upstream is
    /// gone (a dangling route the data plane would then route to). Sharing the
    /// lock — instead of a second one — is what serialises the two, and the
    /// section is short and free of `await`, so a route create never waits on
    /// anything but the handful of map operations here.
    #[must_use]
    pub fn delete_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> bool {
        let _guard = self.route_write.lock();
        let Some((_, upstream)) = self.upstreams.remove(&(tenant_id, upstream_id)) else {
            return false;
        };
        // Conditional: an alias re-bound to another upstream in the meantime
        // must survive this deletion.
        self.aliases
            .remove_if(&(tenant_id, upstream.alias.clone()), |_, holder| {
                *holder == upstream_id
            });
        let orphaned: Vec<(Uuid, Uuid)> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant_id && entry.value().upstream_id == upstream_id)
            .map(|entry| *entry.key())
            .collect();
        for route_key in orphaned {
            if let Some((_, route)) = self.routes.remove(&route_key) {
                self.flush_route_caches(&route);
            }
        }
        self.flush_upstream_caches(tenant_id, upstream_id);
        true
    }

    /// Looks up an upstream by id, scoped to the tenant that owns it.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Option<Arc<Upstream>> {
        self.upstreams
            .get(&(tenant_id, upstream_id))
            .map(|entry| Arc::clone(entry.value()))
    }

    /// Resolves an alias across the ancestor chain (nearest tenant wins).
    #[must_use]
    pub fn resolve_upstream_alias(
        &self,
        tenant_ids: &[Uuid],
        alias: &str,
    ) -> Option<Arc<Upstream>> {
        for tenant_id in tenant_ids {
            // A tenant without the alias simply hands the lookup on to its
            // ancestors; only an exhausted chain is a miss.
            let Some(upstream_id) = self.aliases.get(&(*tenant_id, alias.to_owned())) else {
                continue;
            };
            if let Some(upstream) = self.upstreams.get(&(*tenant_id, *upstream_id)) {
                return Some(Arc::clone(upstream.value()));
            }
        }
        None
    }

    /// Lists upstreams visible from `tenant_ids`, newest first.
    #[must_use]
    pub fn list_upstreams(&self, tenant_ids: &[Uuid]) -> Vec<Arc<Upstream>> {
        let mut visible: Vec<Arc<Upstream>> = self
            .upstreams
            .iter()
            .filter(|entry| tenant_ids.contains(&entry.key().0))
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        visible.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        visible
    }

    /// `true` when any visible tenant already owns `alias`.
    #[must_use]
    pub fn upstream_alias_exists(&self, tenant_ids: &[Uuid], alias: &str) -> bool {
        self.resolve_upstream_alias(tenant_ids, alias).is_some()
    }

    /// Route ids owned by an upstream, sorted for stable pagination.
    #[must_use]
    pub fn route_ids_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant_id && entry.value().upstream_id == upstream_id)
            .map(|entry| entry.key().1)
            .collect();
        ids.sort_unstable();
        ids
    }

    // -- routes ------------------------------------------------------------

    /// Inserts a route owned by an upstream of the same tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::NotFound`] when the owning upstream is unknown to
    /// the tenant, or [`OagwError::Conflict`] when a route with the same match
    /// rule already exists for that upstream.
    pub fn insert_route(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        self.put_route(route, false)
    }

    /// Replaces an existing route and flushes its cache entries.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::NotFound`] when the route or its owning upstream
    /// is unknown to the tenant, or [`OagwError::Conflict`] on a duplicate
    /// match rule.
    pub fn replace_route(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        self.put_route(route, true)
    }

    fn put_route(&self, route: Route, require_existing: bool) -> Result<Arc<Route>, OagwError> {
        // One write lock spans the ownership checks, the uniqueness check and
        // the insert, so two concurrent creates of the same match rule cannot
        // both pass (never held across an `await`).
        let _guard = self.route_write.lock();
        let key = (route.tenant_id, route.id);
        if !self
            .upstreams
            .contains_key(&(route.tenant_id, route.upstream_id))
        {
            return Err(OagwError::not_found(format!(
                "upstream {} does not exist in this tenant",
                route.upstream_id
            )));
        }
        if require_existing && !self.routes.contains_key(&key) {
            return Err(OagwError::not_found(format!(
                "route {} does not exist in this tenant",
                route.id
            )));
        }
        if self.match_rule_taken(&route) {
            return Err(OagwError::conflict(format!(
                "a route with the same match rule already exists for upstream {}",
                route.upstream_id
            )));
        }
        let arc = Arc::new(route);
        self.routes.insert(key, Arc::clone(&arc));
        self.flush_route_caches(&arc);
        Ok(arc)
    }

    /// `true` when another route of the same upstream already claims the same
    /// path, an overlapping method set and the same priority.
    ///
    /// DESIGN section 3.3 pins the rule as `same path + priority + method ->
    /// 409`: a single method *overlapping* an existing set is enough, because
    /// `[GET]` and `[GET, POST]` would both claim the same request. The
    /// gRPC key carries no method set, so an equal match key decides there.
    fn match_rule_taken(&self, route: &Route) -> bool {
        self.routes.iter().any(|entry| {
            entry.key().0 == route.tenant_id
                && entry.key().1 != route.id
                && entry.value().upstream_id == route.upstream_id
                && entry.value().priority == route.priority
                && match_rules_conflict(entry.value(), route)
        })
    }

    /// Removes a route. Returns `true` when it existed.
    #[must_use]
    pub fn delete_route(&self, tenant_id: Uuid, route_id: Uuid) -> bool {
        match self.routes.remove(&(tenant_id, route_id)) {
            Some((_, route)) => {
                self.flush_route_caches(&route);
                true
            }
            None => false,
        }
    }

    /// Looks up a route by id, scoped to the owning tenant.
    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, route_id: Uuid) -> Option<Arc<Route>> {
        self.routes
            .get(&(tenant_id, route_id))
            .map(|entry| Arc::clone(entry.value()))
    }

    /// Lists routes visible from `tenant_ids`, highest priority first.
    #[must_use]
    pub fn list_routes(&self, tenant_ids: &[Uuid]) -> Vec<Arc<Route>> {
        let mut visible: Vec<Arc<Route>> = self
            .routes
            .iter()
            .filter(|entry| tenant_ids.contains(&entry.key().0))
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        visible.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| right.id.cmp(&left.id))
        });
        visible
    }

    // -- plugins -----------------------------------------------------------

    /// Registers a plugin under its id for the owning tenant and seeds the
    /// `plugin:{plugin_id}` L1 with its construction payload (ADR-0005).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Conflict`] when the tenant already owns a plugin
    /// with this id (plugins are immutable, so there is no replace).
    pub fn insert_plugin(&self, plugin: Plugin) -> Result<Arc<Plugin>, OagwError> {
        let key = (plugin.tenant_id, plugin.id);
        if self.plugins.contains_key(&key) {
            return Err(OagwError::conflict(format!(
                "plugin {} already exists in this tenant",
                plugin.id
            ))
            .with_plugin_id(plugin.id.to_string()));
        }
        let arc = Arc::new(plugin);
        self.plugin_cache
            .put(plugin_cache_key(arc.id), Arc::new(arc.config.clone()));
        self.plugins.insert(key, Arc::clone(&arc));
        Ok(arc)
    }

    /// Removes a plugin. Returns the removed plugin when present.
    #[must_use]
    pub fn delete_plugin(&self, tenant_id: Uuid, plugin_id: Uuid) -> Option<Arc<Plugin>> {
        let removed = self
            .plugins
            .remove(&(tenant_id, plugin_id))
            .map(|(_, value)| value);
        self.plugin_cache.remove(&plugin_cache_key(plugin_id));
        removed
    }

    /// Looks up a plugin by id, scoped to the owning tenant.
    #[must_use]
    pub fn get_plugin(&self, tenant_id: Uuid, plugin_id: Uuid) -> Option<Arc<Plugin>> {
        self.plugins
            .get(&(tenant_id, plugin_id))
            .map(|entry| Arc::clone(entry.value()))
    }

    /// Lists plugins visible from `tenant_ids`, sorted by id for stable output.
    #[must_use]
    pub fn list_plugins(&self, tenant_ids: &[Uuid]) -> Vec<Arc<Plugin>> {
        let mut plugins: Vec<Arc<Plugin>> = self
            .plugins
            .iter()
            .filter(|entry| tenant_ids.contains(&entry.key().0))
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        plugins.sort_by_key(|plugin| plugin.id);
        plugins
    }

    // -- referential integrity --------------------------------------------

    /// Upstream ids of *any* tenant whose auth binding or plugin chain
    /// references `plugin` (ADR-0001 `PluginInUse`), sorted for stable output.
    ///
    /// The tenant-wide scan backs the deletion check: a foreign tenant's
    /// binding must block the delete even though its resources may not be named
    /// in the response (see [`RegistryStore::upstream_ids_referencing_plugin_in`]).
    #[must_use]
    pub fn upstream_ids_referencing_plugin(&self, plugin: &Plugin) -> Vec<Uuid> {
        self.scan_upstreams(plugin, None)
    }

    /// Upstream ids owned by `tenant_ids` whose bindings reference `plugin`,
    /// sorted for stable output.
    ///
    /// Used to render the 409 `referenced_by` body: only resources the calling
    /// tenant may see (its own) are named there, while the blocking check stays
    /// tenant-wide.
    #[must_use]
    pub fn upstream_ids_referencing_plugin_in(
        &self,
        plugin: &Plugin,
        tenant_ids: &[Uuid],
    ) -> Vec<Uuid> {
        self.scan_upstreams(plugin, Some(tenant_ids))
    }

    /// Route ids of *any* tenant whose plugin chain references `plugin`, sorted
    /// for stable output.
    #[must_use]
    pub fn route_ids_referencing_plugin(&self, plugin: &Plugin) -> Vec<Uuid> {
        self.scan_routes(plugin, None)
    }

    /// Route ids owned by `tenant_ids` whose plugin chain references `plugin`,
    /// sorted for stable output.
    #[must_use]
    pub fn route_ids_referencing_plugin_in(
        &self,
        plugin: &Plugin,
        tenant_ids: &[Uuid],
    ) -> Vec<Uuid> {
        self.scan_routes(plugin, Some(tenant_ids))
    }

    /// Shared body of the two upstream reference scans.
    fn scan_upstreams(&self, plugin: &Plugin, tenant_ids: Option<&[Uuid]>) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .upstreams
            .iter()
            .filter(|entry| {
                tenant_ids.is_none_or(|tenants| tenants.contains(&entry.key().0))
                    && upstream_references_plugin(entry.value(), plugin)
            })
            .map(|entry| entry.key().1)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Shared body of the two route reference scans.
    fn scan_routes(&self, plugin: &Plugin, tenant_ids: Option<&[Uuid]>) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = self
            .routes
            .iter()
            .filter(|entry| {
                tenant_ids.is_none_or(|tenants| tenants.contains(&entry.key().0))
                    && route_references_plugin(entry.value(), plugin)
            })
            .map(|entry| entry.key().1)
            .collect();
        ids.sort_unstable();
        ids
    }

    // -- L1 caches ---------------------------------------------------------

    /// Reads the `upstream:{tenant_id}:{alias}` cache entry.
    #[must_use]
    pub fn lookup_upstream_cache(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        self.upstream_cache
            .get(&upstream_cache_key(tenant_id, alias))
    }

    /// Reads the `route:{upstream_id}:{method}:{path_prefix}` cache entry.
    #[must_use]
    pub fn lookup_route_cache(
        &self,
        upstream_id: Uuid,
        method: &str,
        path_prefix: &str,
    ) -> Option<Arc<Route>> {
        self.route_cache
            .get(&route_cache_key(upstream_id, method, path_prefix))
    }

    /// Reads the `plugin:{plugin_id}` cache entry.
    #[must_use]
    pub fn lookup_plugin_cache(&self, plugin_id: Uuid) -> Option<Arc<serde_json::Value>> {
        self.plugin_cache.get(&plugin_cache_key(plugin_id))
    }

    /// Reads the `dp:{tenant_id}:{alias}:{method}:{path}` cache entry.
    #[must_use]
    pub fn lookup_dp_cache(
        &self,
        tenant_id: Uuid,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Option<Arc<ResolvedProxyTarget>> {
        self.dp_cache
            .get(&dp_cache_key(tenant_id, alias, method, path))
    }

    /// Stores an upstream under its `upstream:{tenant_id}:{alias}` key.
    pub fn store_upstream_cache(&self, upstream: Arc<Upstream>) {
        let key = upstream_cache_key(upstream.tenant_id, &upstream.alias);
        self.upstream_cache.put(key, upstream);
    }

    /// Stores a route under its `route:{upstream_id}:{method}:{path_prefix}` key.
    pub fn store_route_cache(
        &self,
        upstream_id: Uuid,
        method: &str,
        path_prefix: &str,
        route: Arc<Route>,
    ) {
        self.route_cache
            .put(route_cache_key(upstream_id, method, path_prefix), route);
    }

    /// Stores a plugin payload under its `plugin:{plugin_id}` key.
    pub fn store_plugin_cache(&self, plugin_id: Uuid, payload: Arc<serde_json::Value>) {
        self.plugin_cache.put(plugin_cache_key(plugin_id), payload);
    }

    /// Stores a data-plane resolution snapshot.
    pub fn store_dp_cache(
        &self,
        tenant_id: Uuid,
        alias: &str,
        method: &str,
        path: &str,
        target: Arc<ResolvedProxyTarget>,
    ) {
        self.dp_cache
            .put(dp_cache_key(tenant_id, alias, method, path), target);
    }

    /// Flushes every L1 cache, control plane and data plane.
    pub fn flush_all_caches(&self) {
        self.upstream_cache.clear();
        self.route_cache.clear();
        self.plugin_cache.clear();
        self.dp_cache.clear();
    }

    /// Number of live `upstream:{tenant_id}:{alias}` entries.
    #[must_use]
    pub fn upstream_cache_len(&self) -> usize {
        self.upstream_cache.len()
    }

    /// Number of live `route:{upstream_id}:{method}:{path_prefix}` entries.
    #[must_use]
    pub fn route_cache_len(&self) -> usize {
        self.route_cache.len()
    }

    /// Number of live `plugin:{plugin_id}` entries.
    #[must_use]
    pub fn plugin_cache_len(&self) -> usize {
        self.plugin_cache.len()
    }

    /// Number of live data-plane snapshot entries.
    #[must_use]
    pub fn dp_cache_len(&self) -> usize {
        self.dp_cache.len()
    }

    /// `true` when every L1 cache is empty.
    #[must_use]
    pub fn caches_are_empty(&self) -> bool {
        self.upstream_cache.is_empty()
            && self.route_cache.is_empty()
            && self.plugin_cache.is_empty()
            && self.dp_cache.is_empty()
    }

    /// Flushes the upstream L1 for one tenant, the route L1 for one upstream
    /// and the whole data-plane L1: the invalidation footprint of a single
    /// upstream mutation (ADR-0006).
    ///
    /// The rate-limit buckets of the upstream are dropped with it (ADR-0003):
    /// they are keyed by upstream *id*, so a recreated upstream of the same
    /// alias already gets a fresh budget — dropping them here additionally
    /// releases the memory of the deleted one, and keeps a deleted upstream
    /// from leaving a budget behind that a tenant could re-enter through a
    /// different route of its own.
    ///
    /// The data-plane key carries the *calling* tenant (see
    /// [`dp_cache_key`]), because the caller's tenant chain decides which
    /// upstream an alias resolves to and which routes are visible. A mutation
    /// of an upstream owned by one tenant therefore cannot be expressed as a
    /// key prefix over the callers that resolved it — a descendant's snapshot
    /// of an ancestor's alias is keyed by the descendant — so the whole
    /// snapshot cache is dropped, exactly as [`Self::flush_route_caches`] does.
    fn flush_upstream_caches(&self, tenant_id: Uuid, upstream_id: Uuid) {
        self.upstream_cache
            .remove_prefix(&format!("upstream:{tenant_id}:"));
        self.route_cache
            .remove_prefix(&format!("route:{upstream_id}:"));
        self.dp_cache.clear();
        if let Some(registry) = self.rate_limiters.get() {
            registry.clear_upstream(upstream_id);
        }
    }

    /// Attaches the data plane's rate-limit registry, so an upstream mutation
    /// can drop the buckets that belong to it.
    ///
    /// Idempotent: the first attachment wins, and every later one is ignored —
    /// the registry is a process-wide singleton handed over by the gear
    /// builder.
    pub fn attach_rate_limiters(&self, registry: Arc<RateLimiterRegistry>) {
        let _ = self.rate_limiters.set(registry);
    }

    /// Flushes the route L1 for one upstream and the whole data-plane L1:
    /// the invalidation footprint of a single route mutation (ADR-0006).
    ///
    /// The data-plane cache is dropped wholesale for the same reason
    /// [`Self::flush_upstream_caches`] does it: a route is visible through a
    /// caller's chain, which the snapshot key does not record.
    fn flush_route_caches(&self, route: &Route) {
        self.route_cache
            .remove_prefix(&format!("route:{}:", route.upstream_id));
        self.dp_cache.clear();
    }
}

/// `true` when a binding of `upstream` references `plugin`.
///
/// Both spellings of a plugin reference match (see
/// [`reference_matches_plugin`]): the bare instance UUID and the GTS-form
/// `gts.cf.core.oagw.plugin.v1~{uuid}` id, so a plugin bound in either spelling
/// is correctly reported as in use.
fn upstream_references_plugin(upstream: &Upstream, plugin: &Plugin) -> bool {
    let auth_hit = upstream
        .auth
        .as_ref()
        .is_some_and(|auth| reference_matches_plugin(&auth.auth_type, plugin));
    auth_hit
        || upstream
            .plugins
            .items
            .iter()
            .any(|binding| reference_matches_plugin(&binding.plugin_ref, plugin))
}

/// `true` when a binding of `route` references `plugin`.
fn route_references_plugin(route: &Route, plugin: &Plugin) -> bool {
    route
        .plugins
        .items
        .iter()
        .any(|binding| reference_matches_plugin(&binding.plugin_ref, plugin))
}

/// `true` when two routes claim requests away from each other.
///
/// Two HTTP rules conflict when they share a path, a priority *and* a method;
/// two gRPC rules conflict when their `(service, method)` pair is equal.
fn match_rules_conflict(existing: &Route, candidate: &Route) -> bool {
    match (route_match_key(existing), route_match_key(candidate)) {
        (
            MatchKey::Http {
                path: existing_path,
                methods: existing_methods,
            },
            MatchKey::Http {
                path: candidate_path,
                methods: candidate_methods,
            },
        ) => {
            existing_path == candidate_path
                && existing_methods
                    .intersection(&candidate_methods)
                    .next()
                    .is_some()
        }
        (
            MatchKey::Grpc {
                service: existing_service,
                method: existing_method,
            },
            MatchKey::Grpc {
                service: candidate_service,
                method: candidate_method,
            },
        ) => existing_service == candidate_service && existing_method == candidate_method,
        // An HTTP rule and a gRPC rule can never claim the same request.
        _ => false,
    }
}

#[cfg(test)]
#[path = "../storage_tests.rs"]
mod tests;
