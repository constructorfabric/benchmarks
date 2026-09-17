//! In-process authoritative configuration store.
//!
//! The crate has no `toolkit-db` dependency, so OAGW keeps its configuration
//! in a process-local store created during `Gear::init` and shared with the
//! REST handlers and the data plane through `axum::Extension`. It enforces the
//! same invariants the relational schema would: tenant-scoped identity,
//! `(tenant_id, alias)` uniqueness for upstreams, match-rule and priority
//! uniqueness per upstream, and `(tenant_id, name)` uniqueness for custom
//! plugins.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::model::{CustomPlugin, Route, Upstream};

/// A callback invoked with the id of a deleted upstream and the ids of the
/// routes that went with it.
type UpstreamRemovedHook = Arc<dyn Fn(Uuid, &[Uuid]) + Send + Sync>;

/// The deletion hooks, wrapped so `Store` keeps its derived `Debug`.
#[derive(Default)]
struct Hooks(parking_lot::Mutex<Vec<UpstreamRemovedHook>>);

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("hooks").field(&self.0.lock().len()).finish()
    }
}

/// Authoritative in-process configuration store.
#[derive(Debug, Default)]
pub struct Store {
    upstreams: DashMap<(Uuid, Uuid), Arc<Upstream>>,
    /// Secondary index: `(tenant, normalized alias) -> upstream id`, so the
    /// data plane's per-request lookup does not scan the tenant's upstreams.
    alias_index: DashMap<(Uuid, String), Uuid>,
    routes: DashMap<(Uuid, Uuid), Arc<Route>>,
    /// Secondary index: the routes of an upstream, ordered by position, so a
    /// route match never scans another upstream's routes.
    routes_by_upstream: DashMap<Uuid, Vec<Arc<Route>>>,
    plugins: DashMap<(Uuid, Uuid), Arc<CustomPlugin>>,
    /// Secondary index: `(tenant, plugin name) -> plugin id`, so a definition
    /// cannot be stored twice under one tenant's namespace.
    plugin_names: DashMap<(Uuid, String), Uuid>,
    round_robin: DashMap<Uuid, AtomicU64>,
    positions: AtomicU64,
    hooks: Hooks,
}

impl Store {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a callback invoked with the id of every deleted upstream, and
    /// the ids of the routes that were deleted with it.
    ///
    /// Co-owned derived state (the rate limiter's buckets) invalidates itself
    /// through this hook instead of the store reaching into it.
    pub fn on_upstream_removed(&self, hook: UpstreamRemovedHook) {
        self.hooks.0.lock().push(hook);
    }

    /// Notify the hooks that `upstream` and its `routes` are gone.
    fn upstream_removed(&self, upstream: Uuid, routes: &[Uuid]) {
        for hook in self.hooks.0.lock().iter() {
            hook(upstream, routes);
        }
    }

    // -- upstreams ----------------------------------------------------------

    /// The problem for an alias another upstream of the same tenant owns.
    fn alias_conflict(alias: &str) -> OagwError {
        OagwError::new(
            ErrorKind::Conflict,
            format!("an upstream with alias '{alias}' already exists for this tenant"),
        )
        .with_context("alias", serde_json::json!(alias))
    }

    /// Insert an upstream, enforcing `(tenant_id, alias)` uniqueness.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when another upstream of the same tenant
    /// already owns the alias.
    ///
    /// The claim is made through the alias index's own entry lock, so the
    /// uniqueness check and the insert are one critical section: two concurrent
    /// creations of the same alias cannot both take the vacant slot, and the
    /// record is re-checked once stored so the invariant holds even if a writer
    /// bypassed the index.
    pub fn insert_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        let arc = Arc::new(upstream);
        let key = (arc.tenant_id, crate::domain::alias::normalize(&arc.alias));
        // `or_insert` takes the shard's write lock: the value it finds is either
        // ours (the slot was vacant) or the winner of a concurrent claim.
        let claim = self.alias_index.entry(key.clone()).or_insert(arc.id);
        if *claim != arc.id {
            drop(claim);
            return Err(Self::alias_conflict(&arc.alias));
        }
        self.upstreams
            .insert((arc.tenant_id, arc.id), Arc::clone(&arc));
        // Re-check while the claim is still held: the only way the alias is
        // taken again is an upstream stored under the same normalized spelling
        // by another id, which means the loser must be rolled back.
        if self.alias_stored(arc.tenant_id, &arc.alias, arc.id) {
            drop(claim);
            self.upstreams.remove(&(arc.tenant_id, arc.id));
            self.unclaim(&key, arc.id);
            return Err(Self::alias_conflict(&arc.alias));
        }
        drop(claim);
        Ok(arc)
    }

    /// Replace an upstream in place; the alias must remain unique.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when another upstream already owns the alias.
    pub fn replace_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        let arc = Arc::new(upstream);
        let key = (arc.tenant_id, crate::domain::alias::normalize(&arc.alias));
        let claim = self.alias_index.entry(key.clone()).or_insert(arc.id);
        if *claim != arc.id {
            drop(claim);
            return Err(Self::alias_conflict(&arc.alias));
        }
        let previous = self
            .upstreams
            .insert((arc.tenant_id, arc.id), Arc::clone(&arc));
        // The record moved to a new alias: the index entry it used to hold is
        // retired once the claim is released, because that entry may share the
        // claimed alias's shard.
        let retired = previous
            .as_ref()
            .map(|prior| crate::domain::alias::normalize(&prior.alias))
            .filter(|old_alias| *old_alias != key.1);
        // Re-check while the claim is still held; a loss puts the previous
        // record back, so a failed rename leaves the store as it was.
        if self.alias_stored(arc.tenant_id, &arc.alias, arc.id) {
            drop(claim);
            if let Some(prior) = previous {
                self.upstreams.insert((arc.tenant_id, arc.id), prior);
            } else {
                self.upstreams.remove(&(arc.tenant_id, arc.id));
            }
            self.unclaim(&key, arc.id);
            if let Some(old_alias) = retired {
                self.alias_index
                    .remove_if(&(arc.tenant_id, old_alias), |_, id| *id == arc.id);
            }
            return Err(Self::alias_conflict(&arc.alias));
        }
        drop(claim);
        if let Some(old_alias) = retired {
            self.alias_index
                .remove_if(&(arc.tenant_id, old_alias), |_, id| *id == arc.id);
        }
        Ok(arc)
    }

    /// Release the alias claim `id` holds at `key`.
    ///
    /// Callers drop their entry guard first: removing an entry whose shard lock
    /// the caller still holds would deadlock.
    fn unclaim(&self, key: &(Uuid, String), id: Uuid) {
        self.alias_index.remove_if(key, |_, claim| *claim == id);
    }

    /// Fetch an upstream owned by `tenant`.
    #[must_use]
    pub fn get_upstream(&self, tenant: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        self.upstreams
            .get(&(tenant, id))
            .map(|entry| entry.value().clone())
    }

    /// All upstreams owned by `tenant`, ordered by alias for stable listing.
    #[must_use]
    pub fn upstreams_of(&self, tenant: Uuid) -> Vec<Arc<Upstream>> {
        let mut items: Vec<Arc<Upstream>> = self
            .upstreams
            .iter()
            .filter(|entry| entry.key().0 == tenant)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| a.alias.cmp(&b.alias).then_with(|| a.id.cmp(&b.id)));
        items
    }

    /// The upstream registered for `tenant` under `alias` (already normalized),
    /// read from the alias index.
    #[must_use]
    pub fn upstream_by_alias(&self, tenant: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        let id = *self.alias_index.get(&(tenant, alias.to_owned()))?;
        self.upstreams
            .get(&(tenant, id))
            .map(|entry| entry.value().clone())
    }

    /// The upstream registered under `alias` in any tenant, preferring the
    /// lowest tenant UUID so the answer is deterministic when two tenants
    /// happen to own the same alias.
    ///
    /// The data plane uses this when the request carries no bearer token and
    /// therefore no resolvable tenant: the alias itself then identifies the
    /// configuration and, with it, the tenant.
    #[must_use]
    pub fn upstream_by_alias_any(&self, alias: &str) -> Option<Arc<Upstream>> {
        self.upstreams
            .iter()
            .filter(|entry| entry.value().alias == alias)
            .map(|entry| entry.value().clone())
            .min_by_key(|upstream| upstream.tenant_id)
    }

    /// Delete an upstream and every route belonging to it.
    #[must_use]
    pub fn delete_upstream(&self, tenant: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        let removed = self.upstreams.remove(&(tenant, id)).map(|(_, value)| value);
        if let Some(upstream) = &removed {
            self.alias_index.remove_if(
                &(tenant, crate::domain::alias::normalize(&upstream.alias)),
                |_, index_id| *index_id == id,
            );
            let route_keys: Vec<(Uuid, Uuid)> = self
                .routes
                .iter()
                .filter(|entry| entry.value().upstream_id == id)
                .map(|entry| *entry.key())
                .collect();
            let mut route_ids = Vec::with_capacity(route_keys.len());
            for key in route_keys {
                route_ids.push(key.1);
                self.routes.remove(&key);
            }
            self.routes_by_upstream.remove(&id);
            self.round_robin.remove(&id);
            self.upstream_removed(id, &route_ids);
        }
        removed
    }

    /// Whether another upstream of `tenant` already stores `alias`.
    ///
    /// Reads only the record map: the writers call this while they still hold
    /// the alias index's entry lock for the alias being claimed, and touching
    /// that index again — even under a different key, which can hash to the
    /// same shard — would deadlock.
    fn alias_stored(&self, tenant: Uuid, alias: &str, except: Uuid) -> bool {
        let normalized = crate::domain::alias::normalize(alias);
        self.upstreams.iter().any(|entry| {
            entry.key().0 == tenant
                && entry.value().id != except
                && crate::domain::alias::normalize(&entry.value().alias) == normalized
        })
    }

    // -- routes -------------------------------------------------------------

    /// Insert a route, enforcing match-rule uniqueness inside its upstream.
    ///
    /// # Errors
    ///
    /// Returns a conflict-classified error when an enabled route of the same
    /// upstream already claims the same `(path, priority, method)` combination.
    ///
    /// The per-upstream route list's entry lock is the serialization point: the
    /// conflict scan and the append are one critical section, so two concurrent
    /// identical creations cannot both succeed. The stored record is re-checked
    /// afterwards and the loser rolled back.
    pub fn insert_route(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        let mut route = route;
        let mut siblings = self
            .routes_by_upstream
            .entry(route.upstream_id)
            .or_default();
        if let Some(conflict) = match_conflict_among(&siblings, &route, None) {
            return Err(conflict);
        }
        route.position = self
            .positions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let arc = Arc::new(route);
        siblings.push(Arc::clone(&arc));
        siblings.sort_by_key(|existing| existing.position);
        drop(siblings);
        self.routes
            .insert((arc.tenant_id, arc.id), Arc::clone(&arc));
        // Re-check over the authoritative index (the just-stored route is
        // excluded by id): a second claimant for the same
        // `(path, priority, method)` is rolled back here.
        if let Some(conflict) = self.match_conflict(&arc, None) {
            self.remove_route(&arc);
            return Err(conflict);
        }
        Ok(arc)
    }

    /// Drop a route from both the authoritative map and its upstream's list.
    fn remove_route(&self, route: &Arc<Route>) {
        self.routes.remove(&(route.tenant_id, route.id));
        if let Some(mut siblings) = self.routes_by_upstream.get_mut(&route.upstream_id) {
            siblings.retain(|existing| existing.id != route.id);
        }
    }

    /// Replace a route; `upstream_id` is immutable and the match rule must
    /// stay unique.
    ///
    /// # Errors
    ///
    /// Returns an error when the route does not exist, when `upstream_id`
    /// differs, or when the new match rule or priority collides.
    pub fn replace_route(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        let key = (route.tenant_id, route.id);
        let Some(existing) = self.routes.get(&key).map(|entry| entry.value().clone()) else {
            return Err(OagwError::new(
                ErrorKind::RouteNotFound,
                "route does not exist for this tenant",
            ));
        };
        if existing.upstream_id != route.upstream_id {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "route.upstream_id is immutable",
            ));
        }
        if let Some(conflict) = self.match_conflict(&route, Some(route.id)) {
            return Err(conflict);
        }
        let mut route = route;
        route.position = existing.position;
        let arc = Arc::new(route);
        self.routes.insert(key, Arc::clone(&arc));
        self.index_route(&arc);
        Ok(arc)
    }

    /// Keep the per-upstream index ordered by insertion position.
    ///
    /// Positions are handed out monotonically, so the entry usually appends;
    /// a replacement keeps the position it already had, so it lands where it
    /// was. Sorting the entry in place keeps the invariant regardless.
    fn index_route(&self, route: &Arc<Route>) {
        let mut routes = self
            .routes_by_upstream
            .entry(route.upstream_id)
            .or_default();
        if let Some(slot) = routes.iter_mut().find(|existing| existing.id == route.id) {
            *slot = Arc::clone(route);
        } else {
            routes.push(Arc::clone(route));
        }
        routes.sort_by_key(|existing| existing.position);
        drop(routes);
    }

    /// Fetch a route owned by `tenant`.
    #[must_use]
    pub fn get_route(&self, tenant: Uuid, id: Uuid) -> Option<Arc<Route>> {
        self.routes
            .get(&(tenant, id))
            .map(|entry| entry.value().clone())
    }

    /// Routes owned by `tenant`, ordered by insertion position.
    #[must_use]
    pub fn routes_of(&self, tenant: Uuid) -> Vec<Arc<Route>> {
        let mut items: Vec<Arc<Route>> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by_key(|route| route.position);
        items
    }

    /// Every route belonging to `upstream_id`, ordered by insertion position.
    #[must_use]
    pub fn routes_for_upstream(&self, upstream_id: Uuid) -> Vec<Arc<Route>> {
        self.routes_by_upstream
            .get(&upstream_id)
            .map(|routes| routes.value().clone())
            .unwrap_or_default()
    }

    /// Delete a route.
    #[must_use]
    pub fn delete_route(&self, tenant: Uuid, id: Uuid) -> Option<Arc<Route>> {
        let removed = self.routes.remove(&(tenant, id)).map(|(_, value)| value);
        if let Some(route) = &removed
            && let Some(mut routes) = self.routes_by_upstream.get_mut(&route.upstream_id)
        {
            routes.retain(|existing| existing.id != id);
        }
        removed
    }

    /// Detect a duplicate match rule for `route` among the routes of the same
    /// upstream (excluding `except`).
    fn match_conflict(&self, route: &Route, except: Option<Uuid>) -> Option<OagwError> {
        route.match_rule.http()?;
        let siblings = self.routes_for_upstream(route.upstream_id);
        match_conflict_among(&siblings, route, except)
    }

    // -- plugins ------------------------------------------------------------

    /// The problem for a name another definition of the same tenant owns.
    fn plugin_name_conflict(name: &str) -> OagwError {
        OagwError::new(
            ErrorKind::Conflict,
            format!("a plugin named '{name}' already exists for this tenant"),
        )
        // The offending member is named the way every validation problem names
        // it, so a caller can read `field` without caring about the kind.
        .with_context("field", serde_json::json!("name"))
    }

    /// Insert a custom plugin, enforcing `(tenant_id, name)` uniqueness
    /// (`DESIGN.md` data model: `oagw_plugin` is unique per tenant and name).
    ///
    /// # Errors
    ///
    /// Returns a conflict error when another definition of the same tenant
    /// already owns the name.
    ///
    /// The claim is made through the name index's own entry lock, so the
    /// uniqueness check and the insert are one critical section, exactly like
    /// an upstream alias claim: two concurrent creations of the same name
    /// cannot both take the vacant slot, and the stored records are re-checked
    /// so the invariant holds even if a writer bypassed the index.
    pub fn insert_plugin(&self, plugin: CustomPlugin) -> Result<Arc<CustomPlugin>, OagwError> {
        let arc = Arc::new(plugin);
        let key = (arc.tenant_id, arc.name.trim().to_owned());
        // `or_insert` takes the shard's write lock: the value it finds is either
        // ours (the slot was vacant) or the winner of a concurrent claim.
        let claim = self.plugin_names.entry(key.clone()).or_insert(arc.id);
        if *claim != arc.id {
            drop(claim);
            return Err(Self::plugin_name_conflict(&arc.name));
        }
        self.plugins
            .insert((arc.tenant_id, arc.id), Arc::clone(&arc));
        // Re-check while the claim is still held: the only way the name is taken
        // again is another id stored under the same spelling, and that loser is
        // rolled back.
        if self.plugin_name_stored(arc.tenant_id, &arc.name, arc.id) {
            drop(claim);
            self.plugins.remove(&(arc.tenant_id, arc.id));
            self.plugin_names
                .remove_if(&key, |_, claim| *claim == arc.id);
            return Err(Self::plugin_name_conflict(&arc.name));
        }
        drop(claim);
        Ok(arc)
    }

    /// Whether another plugin of `tenant` already stores `name`.
    ///
    /// Reads only the record map: the writer calls this while it still holds
    /// the name index's entry lock, and touching that index again — even under
    /// a different key, which can hash to the same shard — would deadlock.
    fn plugin_name_stored(&self, tenant: Uuid, name: &str, except: Uuid) -> bool {
        self.plugins.iter().any(|entry| {
            entry.key().0 == tenant
                && entry.value().id != except
                && entry.value().name.trim() == name.trim()
        })
    }

    /// Fetch a custom plugin owned by `tenant`.
    #[must_use]
    pub fn get_plugin(&self, tenant: Uuid, id: Uuid) -> Option<Arc<CustomPlugin>> {
        self.plugins
            .get(&(tenant, id))
            .map(|entry| entry.value().clone())
    }

    /// All custom plugins owned by `tenant`.
    #[must_use]
    pub fn plugins_of(&self, tenant: Uuid) -> Vec<Arc<CustomPlugin>> {
        let mut items: Vec<Arc<CustomPlugin>> = self
            .plugins
            .iter()
            .filter(|entry| entry.key().0 == tenant)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        items
    }

    /// Delete a plugin, reporting which upstreams and routes still reference it.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::PluginInUse`] with the referencing resources.
    pub fn delete_plugin(&self, tenant: Uuid, id: Uuid) -> Result<Arc<CustomPlugin>, OagwError> {
        let mut referenced_by_upstreams: Vec<String> = Vec::new();
        let mut referenced_by_routes: Vec<String> = Vec::new();

        for upstream in self.upstreams_of(tenant) {
            if references_plugin(&upstream.plugins.items, id)
                || upstream.auth.as_ref().is_some_and(|auth| {
                    auth.plugin_type
                        .as_deref()
                        .is_some_and(|reference| uuid_matches(reference, id))
                })
            {
                referenced_by_upstreams.push(crate::domain::model::gts_id(
                    crate::domain::model::UPSTREAM_GTS_BASE,
                    upstream.id,
                ));
            }
        }
        for route in self.routes_of(tenant) {
            if references_plugin(&route.plugins.items, id) {
                referenced_by_routes.push(crate::domain::model::gts_id(
                    crate::domain::model::ROUTE_GTS_BASE,
                    route.id,
                ));
            }
        }

        if !referenced_by_upstreams.is_empty() || !referenced_by_routes.is_empty() {
            return Err(OagwError::new(
                ErrorKind::PluginInUse,
                format!(
                    "Plugin is referenced by {} upstream(s) and {} route(s)",
                    referenced_by_upstreams.len(),
                    referenced_by_routes.len()
                ),
            )
            .with_context(
                "plugin_id",
                serde_json::Value::String(crate::domain::model::gts_id(
                    crate::domain::model::PLUGIN_GTS_BASE,
                    id,
                )),
            )
            .with_context(
                "referenced_by",
                serde_json::json!({
                    "upstreams": referenced_by_upstreams,
                    "routes": referenced_by_routes,
                }),
            ));
        }

        self.plugins
            .remove(&(tenant, id))
            .map(|(_, value)| {
                // The name becomes claimable again: the index entry is dropped
                // only when it still points at the deleted definition.
                self.plugin_names
                    .remove_if(&(tenant, value.name.trim().to_owned()), |_, claim| {
                        *claim == id
                    });
                value
            })
            .ok_or_else(|| {
                OagwError::new(
                    ErrorKind::RouteNotFound,
                    "plugin does not exist for this tenant",
                )
            })
    }

    // -- data plane helpers -------------------------------------------------

    /// Advance and return the round-robin cursor for `upstream_id`.
    #[must_use]
    pub fn next_round_robin(&self, upstream_id: Uuid) -> u64 {
        self.round_robin
            .entry(upstream_id)
            .or_default()
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

fn match_rule_path(route: &Route) -> String {
    route
        .match_rule
        .http()
        .map_or_else(String::new, |http| http.path.clone())
}

/// The conflict a duplicate `(path, priority, method)` claim among `siblings`
/// produces, or `None` when `route` is free to be stored.
///
/// Shared by the insert and replace paths so both classify the collision the
/// same way: two *enabled* routes of one upstream may not share a path,
/// priority, and method (`DESIGN.md` §CRUD, "same path + priority + method →
/// 409"), and a well-formed request that cannot be applied is a `409`, not a
/// validation failure. Two routes on the same path at *different* priorities
/// are both kept: the higher priority is the one the matcher serves. A disabled
/// route is matched by nothing, so it claims nothing — enabling it later is the
/// act that has to clear the check, and the replace path runs it.
fn match_conflict_among(
    siblings: &[Arc<Route>],
    route: &Route,
    except: Option<Uuid>,
) -> Option<OagwError> {
    if route.match_rule.http().is_none() || !route.enabled {
        return None;
    }
    let path = normalize_pattern(&match_rule_path(route));
    let overlap = siblings
        .iter()
        .filter(|existing| existing.id != route.id && except.is_none_or(|id| id != existing.id))
        .filter(|existing| existing.enabled)
        .filter(|existing| normalized_path(existing) == path)
        .filter(|existing| existing.priority == route.priority)
        .any(|existing| {
            existing.match_rule.http().is_some_and(|http| {
                http.methods
                    .iter()
                    .any(|method| match_rule_methods(route).contains(method))
            })
        });
    overlap.then(|| {
        OagwError::new(
            ErrorKind::Conflict,
            "another route of this upstream already matches the same path, priority and method",
        )
        .with_context("path", serde_json::json!(path))
        .with_context("priority", serde_json::json!(route.priority))
    })
}

fn match_rule_methods(route: &Route) -> Vec<crate::domain::model::HttpMethod> {
    route
        .match_rule
        .http()
        .map_or_else(Vec::new, |http| http.methods.clone())
}

fn normalized_path(route: &Route) -> String {
    normalize_pattern(&match_rule_path(route))
}

/// Normalize a route path pattern: lowercase, no trailing slash.
#[must_use]
pub fn normalize_pattern(path: &str) -> String {
    let trimmed = path.trim();
    let stripped = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if stripped.is_empty() {
        return "/".to_owned();
    }
    stripped.to_ascii_lowercase()
}

/// Whether `items` references the plugin `id`.
fn references_plugin(items: &[crate::domain::model::PluginBindingDto], id: Uuid) -> bool {
    items.iter().any(|binding| match binding {
        crate::domain::model::PluginBindingDto::Ref(reference) => uuid_matches(reference, id),
        crate::domain::model::PluginBindingDto::Detailed {
            plugin_ref,
            config: _,
        } => uuid_matches(plugin_ref, id),
    })
}

/// Whether `reference` is a UUID equal to `id`.
fn uuid_matches(reference: &str, id: Uuid) -> bool {
    Uuid::parse_str(reference.trim()).is_ok_and(|parsed| parsed == id)
}
