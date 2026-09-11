// Created: 2026-09-01 by Constructor Tech
//! In-memory Control Plane store.
//!
//! `docs/DESIGN.md` §3.6 notes that the gear's `Cargo.toml` carries no
//! `toolkit-db` dependency, so state lives in process memory keyed by
//! tenant and is lost on restart — the documented single-exec deployment
//! mode (`docs/../src/lib.rs`).

use std::collections::BTreeMap;
use std::sync::Arc;

use parking_lot::RwLock;

use super::model::{Plugin, Route, Upstream, normalize_alias, uuid_from_resource_id};

/// Per-tenant tables.
#[derive(Debug, Default)]
pub struct TenantTables {
    /// By alias (normalized).
    pub upstreams_by_alias: BTreeMap<String, String>,
    /// By id.
    pub upstreams: BTreeMap<String, Upstream>,
    /// By id.
    pub routes: BTreeMap<String, Route>,
    /// By id.
    pub plugins: BTreeMap<String, Plugin>,
    /// Plugins that became unlinked, awaiting garbage collection.
    pub unlinked_since: BTreeMap<String, std::time::Instant>,
}

/// The whole roster, keyed by tenant id.
#[derive(Debug, Default)]
struct Tables {
    tenants: dashmap::DashMap<String, Arc<RwLock<TenantTables>>>,
}

impl TenantTables {
    /// Routes of one upstream, longest-prefix first.
    #[must_use]
    pub fn routes_for(&self, upstream_id: &str) -> Vec<&Route> {
        let mut routes: Vec<&Route> = self
            .routes
            .values()
            .filter(|r| r.upstream_id == upstream_id && r.enabled)
            .collect();
        routes.sort_by_key(|a| route_order(a));
        routes
    }
}

/// The key a resource reference is stored under.
///
/// Resources are keyed by their full GTS identifier, so a bare UUID — or a
/// full identifier whose tail it is — resolves by suffix match. Callers hand
/// this whatever the path parameter carried.
fn resolve_key<T>(resources: &BTreeMap<String, T>, raw: &str) -> Option<String> {
    if resources.contains_key(raw) {
        return Some(raw.to_owned());
    }
    let uuid = uuid_from_resource_id(raw)?;
    resources
        .keys()
        .find(|key| key.ends_with(&format!("~{uuid}")))
        .cloned()
}

/// Sort key implementing "longest path prefix, then higher priority".
fn route_order(route: &Route) -> (std::cmp::Reverse<usize>, std::cmp::Reverse<i64>, String) {
    let path_len = route
        .matcher
        .http
        .as_ref()
        .map(|h| h.path.len())
        .unwrap_or(0);
    (
        std::cmp::Reverse(path_len),
        std::cmp::Reverse(route.priority),
        route.id.clone(),
    )
}

/// The Control Plane store.
#[derive(Debug, Default)]
pub struct Store {
    tables: Tables,
}

impl Store {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn tenant(&self, tenant_id: &str) -> Arc<RwLock<TenantTables>> {
        self.tables
            .tenants
            .entry(tenant_id.to_owned())
            .or_insert_with(|| Arc::new(RwLock::new(TenantTables::default())))
            .clone()
    }

    /// Read-only view of one tenant's tables, if it has any.
    #[must_use]
    pub fn tenant_tables(&self, tenant_id: &str) -> Option<Arc<RwLock<TenantTables>>> {
        self.tables
            .tenants
            .get(tenant_id)
            .map(|e| Arc::clone(e.value()))
    }

    // ---- upstreams ----------------------------------------------------

    /// Insert an upstream, rejecting a duplicate alias.
    ///
    /// # Errors
    /// Returns a conflict message when the tenant already owns an upstream
    /// with the same alias.
    pub fn insert_upstream(&self, upstream: Upstream) -> Result<Upstream, String> {
        let alias = normalize_alias(&upstream.alias);
        let tenant = self.tenant(&upstream.tenant_id);
        let mut tables = tenant.write();
        if tables.upstreams_by_alias.contains_key(&alias) {
            return Err(format!("upstream '{alias}' already exists"));
        }
        tables.upstreams_by_alias.insert(alias, upstream.id.clone());
        tables
            .upstreams
            .insert(upstream.id.clone(), upstream.clone());
        Ok(upstream)
    }

    /// Replace an upstream in place.
    ///
    /// # Errors
    /// Returns a conflict message when another of this tenant's upstreams
    /// already owns the alias.
    pub fn put_upstream(&self, upstream: Upstream) -> Result<(), String> {
        let alias = normalize_alias(&upstream.alias);
        let tenant = self.tenant(&upstream.tenant_id);
        let mut tables = tenant.write();
        let old_alias = tables
            .upstreams
            .get(&upstream.id)
            .map(|old| normalize_alias(&old.alias));
        if old_alias.as_deref() != Some(alias.as_str())
            && tables
                .upstreams_by_alias
                .get(&alias)
                .is_some_and(|owner| *owner != upstream.id)
        {
            return Err(format!("upstream '{alias}' already exists"));
        }
        if let Some(old_alias) = old_alias.filter(|old| *old != alias) {
            tables.upstreams_by_alias.remove(&old_alias);
        }
        tables
            .upstreams_by_alias
            .insert(alias.clone(), upstream.id.clone());
        tables.upstreams.insert(upstream.id.clone(), upstream);
        Ok(())
    }

    /// Fetch one of this tenant's upstreams by id.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: &str, id: &str) -> Option<Upstream> {
        let tables = self.tenant(tenant_id);
        let read = tables.read();
        read.upstreams
            .get(&resolve_key(&read.upstreams, id)?)
            .cloned()
    }

    /// Fetch one of this tenant's upstreams by normalized alias.
    #[must_use]
    pub fn get_upstream_by_alias(&self, tenant_id: &str, alias: &str) -> Option<Upstream> {
        let tables = self.tenant(tenant_id);
        let read = tables.read();
        let id = read.upstreams_by_alias.get(&normalize_alias(alias))?;
        read.upstreams.get(id).cloned()
    }

    /// Delete an upstream owned by `tenant_id`, returning it when found.
    pub fn delete_upstream(&self, tenant_id: &str, id: &str) -> Option<Upstream> {
        let tenant = self.tenant(tenant_id);
        let mut tables = tenant.write();
        let key = resolve_key(&tables.upstreams, id)?;
        let removed = tables.upstreams.remove(&key)?;
        tables
            .upstreams_by_alias
            .remove(&normalize_alias(&removed.alias));
        tables.routes.retain(|_, r| r.upstream_id != key);
        Some(removed)
    }

    /// All upstreams owned by `tenant_id`, in alias order.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: &str) -> Vec<Upstream> {
        let tables = self.tenant(tenant_id);
        let tables = tables.read();
        let mut items: Vec<Upstream> = tables.upstreams.values().cloned().collect();
        items.sort_by(|a, b| a.alias.cmp(&b.alias).then(a.id.cmp(&b.id)));
        items
    }

    // ---- routes -------------------------------------------------------

    /// Insert a route, rejecting a duplicate match rule.
    ///
    /// # Errors
    /// Returns a conflict message when the upstream already has a route with
    /// the same path, priority and method set.
    pub fn insert_route(&self, route: Route) -> Result<Route, String> {
        let tenant = self.tenant(&route.tenant_id);
        let mut tables = tenant.write();
        if tables
            .routes
            .values()
            .any(|r| r.upstream_id == route.upstream_id && same_match(r, &route))
        {
            return Err(format!(
                "upstream '{}' already has a route matching this path, priority and methods",
                route.upstream_id
            ));
        }
        tables.routes.insert(route.id.clone(), route.clone());
        Ok(route)
    }

    /// Replace a route. `upstream_id` is immutable.
    pub fn put_route(&self, route: Route) {
        let tenant = self.tenant(&route.tenant_id);
        let mut tables = tenant.write();
        tables.routes.insert(route.id.clone(), route);
    }

    /// Fetch one of this tenant's routes by id.
    #[must_use]
    pub fn get_route(&self, tenant_id: &str, id: &str) -> Option<Route> {
        let tables = self.tenant(tenant_id);
        let read = tables.read();
        read.routes.get(&resolve_key(&read.routes, id)?).cloned()
    }

    /// Delete a route owned by `tenant_id`.
    pub fn delete_route(&self, tenant_id: &str, id: &str) -> Option<Route> {
        let tables = self.tenant(tenant_id);
        let mut tables = tables.write();
        let key = resolve_key(&tables.routes, id)?;
        tables.routes.remove(&key)
    }

    /// All routes owned by `tenant_id`.
    #[must_use]
    pub fn list_routes(&self, tenant_id: &str) -> Vec<Route> {
        let tables = self.tenant(tenant_id);
        let tables = tables.read();
        let mut items: Vec<Route> = tables.routes.values().cloned().collect();
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items
    }

    // ---- plugins ------------------------------------------------------

    /// Insert a plugin.
    pub fn insert_plugin(&self, plugin: Plugin) -> Plugin {
        let tenant = self.tenant(&plugin.tenant_id);
        let mut tables = tenant.write();
        tables.unlinked_since.remove(&plugin.id);
        tables.plugins.insert(plugin.id.clone(), plugin.clone());
        plugin
    }

    /// Fetch one of this tenant's plugins by id.
    #[must_use]
    pub fn get_plugin(&self, tenant_id: &str, id: &str) -> Option<Plugin> {
        let tables = self.tenant(tenant_id);
        let read = tables.read();
        read.plugins.get(&resolve_key(&read.plugins, id)?).cloned()
    }

    /// Delete a plugin owned by `tenant_id` when nothing references it.
    ///
    /// # Errors
    /// Returns the referencing resource ids when the plugin is in use.
    pub fn delete_plugin(&self, tenant_id: &str, id: &str) -> Result<Plugin, Vec<String>> {
        let tenant = self.tenant(tenant_id);
        let mut tables = tenant.write();
        let Some(key) = resolve_key(&tables.plugins, id) else {
            return Err(Vec::new());
        };
        let mut refs: Vec<String> = Vec::new();
        for u in tables.upstreams.values() {
            if plugin_bound_to_upstream(u, &key) {
                refs.push(format!("upstream:{}", u.id));
            }
        }
        for r in tables.routes.values() {
            if plugin_bound_to_route(r, &key) {
                refs.push(format!("route:{}", r.id));
            }
        }
        if !refs.is_empty() {
            return Err(refs);
        }
        tables.plugins.remove(&key).ok_or_else(Vec::new)
    }

    /// All plugins owned by `tenant_id`.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: &str) -> Vec<Plugin> {
        let tables = self.tenant(tenant_id);
        let tables = tables.read();
        let mut items: Vec<Plugin> = tables.plugins.values().cloned().collect();
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items
    }

    /// Mark `id` as unlinked when nothing references it any more.
    pub fn mark_unlinked_if_orphaned(&self, tenant_id: &str, id: &str) {
        let tenant = self.tenant(tenant_id);
        let mut tables = tenant.write();
        let Some(key) = resolve_key(&tables.plugins, id) else {
            return;
        };
        let still_linked = tables
            .upstreams
            .values()
            .any(|u| plugin_bound_to_upstream(u, &key))
            || tables
                .routes
                .values()
                .any(|r| plugin_bound_to_route(r, &key));
        if still_linked {
            tables.unlinked_since.remove(&key);
        } else {
            tables.unlinked_since.insert(key, std::time::Instant::now());
        }
    }
}

pub fn plugin_bound_to_upstream(u: &Upstream, plugin_id: &str) -> bool {
    u.plugins.items.iter().any(|b| b.id() == plugin_id)
        || u.auth
            .as_ref()
            .and_then(|a| a.auth_type.as_deref())
            .is_some_and(|t| t == plugin_id)
}

pub fn plugin_bound_to_route(r: &Route, plugin_id: &str) -> bool {
    r.plugins.items.iter().any(|b| b.id() == plugin_id)
}

/// `true` when two routes match on path, priority and method set.
fn same_match(a: &Route, b: &Route) -> bool {
    match (&a.matcher.http, &b.matcher.http) {
        (Some(x), Some(y)) => {
            x.path == y.path && a.priority == b.priority && x.methods.iter().eq(y.methods.iter())
        }
        (None, None) => {
            a.matcher.grpc.as_ref().map(|g| (&g.service, &g.method))
                == b.matcher.grpc.as_ref().map(|g| (&g.service, &g.method))
        }
        _ => false,
    }
}

/// A shared store handle.
pub type SharedStore = Arc<Store>;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, HttpMatch, PathSuffixMode, RouteMatch, ServerConfig};

    fn upstream(alias: &str, tenant: &str) -> Upstream {
        Upstream {
            id: format!("u-{alias}"),
            tenant_id: tenant.to_owned(),
            alias: alias.to_owned(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            ..Upstream::default()
        }
    }

    fn route(id: &str, upstream_id: &str, path: &str) -> Route {
        Route {
            id: id.to_owned(),
            tenant_id: "t1".to_owned(),
            upstream_id: upstream_id.to_owned(),
            matcher: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            ..Route::default()
        }
    }

    #[test]
    fn alias_is_unique_per_tenant() {
        let store = Store::new();
        assert!(
            store
                .insert_upstream(upstream("api.openai.com", "t1"))
                .is_ok()
        );
        assert!(
            store
                .insert_upstream(upstream("api.openai.com", "t1"))
                .is_err()
        );
        // A different tenant may reuse the alias.
        assert!(
            store
                .insert_upstream(upstream("api.openai.com", "t2"))
                .is_ok()
        );
    }

    #[test]
    fn alias_lookup_is_case_insensitive() {
        let store = Store::new();
        store
            .insert_upstream(upstream("api.openai.com", "t1"))
            .expect("insert");
        assert!(
            store
                .get_upstream_by_alias("t1", "Api.OpenAI.COM")
                .is_some()
        );
        assert!(
            store
                .get_upstream_by_alias("t1", "api.openai.com.")
                .is_some()
        );
        assert!(
            store
                .get_upstream_by_alias("t2", "api.openai.com")
                .is_none()
        );
    }

    #[test]
    fn put_upstream_reindexes_when_the_alias_moves() {
        let store = Store::new();
        let mut u = upstream("api.openai.com", "t1");
        store.put_upstream(u.clone()).expect("first insert");
        u.alias = "api.anthropic.com".to_owned();
        store.put_upstream(u.clone()).expect("rename");
        assert!(
            store
                .get_upstream_by_alias("t1", "api.openai.com")
                .is_none()
        );
        assert!(
            store
                .get_upstream_by_alias("t1", "api.anthropic.com")
                .is_some()
        );
    }

    #[test]
    fn put_upstream_refuses_an_alias_another_upstream_owns() {
        let store = Store::new();
        store
            .put_upstream(upstream("api.openai.com", "t1"))
            .expect("first insert");
        let mut other = upstream("api.anthropic.com", "t1");
        other.id = "u-other".to_owned();
        store.put_upstream(other).expect("second insert");
        let mut renamed = store
            .get_upstream("t1", "u-other")
            .expect("second upstream");
        renamed.alias = "api.openai.com".to_owned();
        assert!(store.put_upstream(renamed).is_err());
        // The owner keeps the alias.
        assert!(
            store
                .get_upstream_by_alias("t1", "api.openai.com")
                .is_some_and(|u| u.id == "u-api.openai.com")
        );
    }

    #[test]
    fn a_resource_is_reachable_by_its_bare_uuid() {
        let store = Store::new();
        let stored = store
            .insert_upstream(upstream("api.openai.com", "t1"))
            .expect("first insert");
        let id = stored.id.clone();
        let uuid = id.rsplit('~').next().unwrap_or("").to_owned();
        assert_eq!(
            store.get_upstream("t1", &id).map(|u| u.id),
            Some(id.clone())
        );
        assert_eq!(
            store.get_upstream("t1", &uuid).map(|u| u.id),
            Some(id.clone())
        );
        assert!(store.get_upstream("t2", &id).is_none(), "other tenant");
    }

    #[test]
    fn delete_upstream_cascades_to_routes() {
        let store = Store::new();
        store
            .put_upstream(upstream("api.openai.com", "t1"))
            .expect("first insert");
        store
            .insert_route(route("r1", "u-api.openai.com", "/v1"))
            .expect("inserted");
        assert!(store.get_route("t1", "r1").is_some());
        store.delete_upstream("t1", "u-api.openai.com");
        assert!(store.get_route("t1", "r1").is_none());
    }

    #[test]
    fn routes_are_ordered_by_longest_prefix_then_priority() {
        let store = Store::new();
        let mut deep = route("r-deep", "u1", "/v1/chat/completions");
        deep.priority = 0;
        let mut shallow = route("r-shallow", "u1", "/v1");
        shallow.priority = 99;
        let mut equal = route("r-equal", "u1", "/v1");
        equal.priority = 5;
        store.insert_route(deep).expect("unique");
        store.insert_route(shallow).expect("unique");
        store.insert_route(equal).expect("unique");
        let tables = store.tenant_tables("t1").expect("tenant");
        let guard = tables.read();
        let ordered = guard.routes_for("u1");
        let ids: Vec<&str> = ordered.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["r-deep", "r-shallow", "r-equal"]);
    }

    #[test]
    fn duplicate_match_rule_is_rejected() {
        let store = Store::new();
        store.insert_route(route("r1", "u1", "/v1")).expect("first");
        assert!(store.insert_route(route("r2", "u1", "/v1")).is_err());
        // A different path is fine.
        assert!(store.insert_route(route("r3", "u1", "/v2")).is_ok());
        // A different upstream is fine.
        assert!(store.insert_route(route("r4", "u2", "/v1")).is_ok());
    }

    #[test]
    fn put_route_keeps_upstream_binding_fresh() {
        let store = Store::new();
        store.insert_route(route("r1", "u1", "/v1")).expect("first");
        let mut updated = route("r1", "u1", "/v2");
        updated.enabled = false;
        store.put_route(updated);
        let stored = store.get_route("t1", "r1").expect("stored");
        assert!(!stored.enabled);
        assert_eq!(stored.matcher.http.expect("http").path, "/v2");
    }

    #[test]
    fn plugin_deletion_is_blocked_while_referenced() {
        let store = Store::new();
        let mut u = upstream("api.openai.com", "t1");
        u.plugins.items = vec![crate::domain::model::PluginBinding::Ref(
            "p-custom".to_owned(),
        )];
        store.put_upstream(u).expect("first insert");
        store.insert_plugin(crate::domain::model::Plugin {
            id: "p-custom".to_owned(),
            tenant_id: "t1".to_owned(),
            plugin_type: "guard".to_owned(),
            name: Some("custom".to_owned()),
            source: Some("def guard(ctx): return ctx".to_owned()),
            config: BTreeMap::new(),
        });
        assert!(store.delete_plugin("t1", "p-custom").is_err());
        let mut stored = store.get_upstream("t1", "u-api.openai.com").expect("u");
        stored.plugins.items.clear();
        store.put_upstream(stored).expect("update");
        store.mark_unlinked_if_orphaned("t1", "p-custom");
        assert!(store.delete_plugin("t1", "p-custom").is_ok());
    }

    #[test]
    fn a_plugin_bound_by_its_full_identifier_cannot_be_deleted() {
        let store = Store::new();
        let id = "gts.cf.core.oagw.guard_plugin.v1~47b8d4e9-9fe8-4a0a-8001-3a5cae000b41";
        let mut u = upstream("api.openai.com", "t1");
        u.plugins.items = vec![crate::domain::model::PluginBinding::Ref(id.to_owned())];
        store.put_upstream(u).expect("first insert");
        store.insert_plugin(crate::domain::model::Plugin {
            id: id.to_owned(),
            tenant_id: "t1".to_owned(),
            plugin_type: "guard".to_owned(),
            name: Some("custom".to_owned()),
            source: Some("def guard(ctx): return ctx".to_owned()),
            config: BTreeMap::new(),
        });
        // By the full identifier, and by the bare UUID the identifier ends with.
        assert_eq!(
            store.delete_plugin("t1", id).unwrap_err(),
            vec![format!("upstream:{}", "u-api.openai.com")]
        );
        assert!(store.get_plugin("t1", id).is_some());
        assert_eq!(
            store
                .delete_plugin("t1", "47b8d4e9-9fe8-4a0a-8001-3a5cae000b41")
                .unwrap_err()
                .len(),
            1
        );
        assert!(store.get_plugin("t1", id).is_some(), "still bound");
    }

    #[test]
    fn ancestor_upstreams_are_invisible_to_the_management_api() {
        let store = Store::new();
        store
            .insert_upstream(upstream("api.openai.com", "root"))
            .expect("insert");
        assert!(store.get_upstream("leaf", "u-api.openai.com").is_none());
        assert!(store.list_upstreams("leaf").is_empty());
        assert!(
            store
                .get_upstream_by_alias("leaf", "api.openai.com")
                .is_none()
        );
    }
}
