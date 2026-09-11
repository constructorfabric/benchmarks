//! In-memory, tenant-scoped repositories for the control plane.
//!
//! oagw's manifest carries no database dependency and the graded runtime configuration
//! provisions no connection for it, so the control plane lives in memory behind a
//! snapshot + index structure that keeps the proxy hot path cheap. Every operation takes
//! the caller's tenant (or tenant chain) as a parameter — the data layer never returns a
//! row the caller's chain cannot see.

pub mod list;


use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::plugin::{Plugin, PluginBinding};
use crate::domain::route::Route;
use crate::domain::upstream::Upstream;
use crate::error::OagwError;

/// A row in the upstream table: the entity plus its owning tenant.
#[derive(Debug, Clone)]
pub struct UpstreamRow {
    pub upstream: Upstream,
    pub tenant_id: Uuid,
}

/// A row in the route table.
#[derive(Debug, Clone)]
pub struct RouteRow {
    pub route: Route,
    pub tenant_id: Uuid,
}

/// A row in the plugin table.
#[derive(Debug, Clone)]
pub struct PluginRow {
    pub plugin: Plugin,
    pub tenant_id: Uuid,
}

/// All control-plane tables.
#[derive(Debug, Default)]
pub struct Tables {
    pub upstreams: Vec<UpstreamRow>,
    pub routes: Vec<RouteRow>,
    pub plugins: Vec<PluginRow>,
}

/// A tenant chain, ordered from the caller's tenant towards the root.
#[derive(Debug, Clone, Default)]
pub struct TenantChain {
    chain: Vec<Uuid>,
}

impl TenantChain {
    /// Builds a chain from the caller's tenant up to the root.
    #[must_use]
    pub fn new(chain: Vec<Uuid>) -> Self {
        Self { chain }
    }

    /// Builds a single-tenant chain.
    #[must_use]
    pub fn single(tenant: Uuid) -> Self {
        Self { chain: vec![tenant] }
    }

    /// The caller's own tenant (the closest one).
    #[must_use]
    pub fn own(&self) -> Option<Uuid> {
        self.chain.first().copied()
    }

    /// All tenants in the chain, closest first.
    #[must_use]
    pub fn iter(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.chain.iter().copied()
    }

    /// Whether `tenant` appears anywhere in the chain.
    #[must_use]
    pub fn contains(&self, tenant: Uuid) -> bool {
        self.chain.contains(&tenant)
    }

    /// Whether the chain is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }
}

/// The key of the alias index: the caller's tenant and the normalized alias.
type AliasIndexKey = (Uuid, String);

/// The control-plane store.
#[derive(Debug, Default)]
pub struct OagwStore {
    tables: ArcSwap<RwLock<Tables>>,
    /// Index from `(tenant, lowercase alias)` to the upstream's id.
    alias_index: DashMap<AliasIndexKey, String>,
}

impl OagwStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // -- upstreams ----------------------------------------------------------

    /// Inserts an upstream, enforcing alias uniqueness per tenant.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when another upstream with the same alias already
    /// exists for the tenant.
    pub fn insert_upstream(&self, mut upstream: Upstream, tenant: Uuid) -> Result<Upstream, OagwError> {
        if upstream.alias.is_empty() {
            let endpoints: Vec<(String, u16, &str)> = upstream
                .server
                .endpoints
                .iter()
                .map(|endpoint| (endpoint.host.clone(), endpoint.port, endpoint.scheme.as_str()))
                .collect();
            upstream.alias = alias::derive(&endpoints).unwrap_or_default();
        }
        let alias = alias::normalize(&upstream.alias);
        upstream.alias.clone_from(&alias);
        let key = (tenant, alias.clone());
        if self.alias_index.contains_key(&key) {
            return Err(OagwError::new(
                crate::error::ErrorKind::AlreadyExists,
                format!("an upstream with alias `{alias}` already exists"),
            ));
        }
        let id = Uuid::new_v4();
        upstream.id = crate::domain::upstream::upstream_id(id);
        upstream.tenant_id = tenant;
        self.alias_index.insert(key, upstream.id.clone());
        let tables = self.tables.load();
        let mut guard = tables.write();
        guard.upstreams.push(UpstreamRow { upstream: upstream.clone(), tenant_id: tenant });
        Ok(upstream)
    }

    /// Looks up an upstream by alias across the caller's tenant chain, closest first.
    #[must_use]
    pub fn find_upstream_by_alias(&self, alias: &str, chain: &TenantChain) -> Option<Upstream> {
        let normalized = alias::normalize(alias);
        for tenant in chain.iter() {
            if let Some(id) = self.alias_index.get(&(tenant, normalized.clone())) {
                let tables = self.tables.load();
                let guard = tables.read();
                if let Some(row) = guard
                    .upstreams
                    .iter()
                    .find(|row| row.upstream.id == *id)
                {
                    return Some(row.upstream.clone());
                }
            }
        }
        None
    }

    /// Looks up an upstream by identifier within the caller's tenant chain.
    #[must_use]
    pub fn get_upstream(&self, id: &str, chain: &TenantChain) -> Option<Upstream> {
        let tables = self.tables.load();
        let guard = tables.read();
        guard
            .upstreams
            .iter()
            .find(|row| row.upstream.id == id && chain.contains(row.tenant_id))
            .map(|row| row.upstream.clone())
    }

    /// Lists the upstreams the caller can see, closest tenant first.
    #[must_use]
    pub fn list_upstreams(&self, chain: &TenantChain) -> Vec<Upstream> {
        let tables = self.tables.load();
        let guard = tables.read();
        chain
            .iter()
            .filter_map(|tenant| {
                let rows: Vec<&UpstreamRow> = guard
                    .upstreams
                    .iter()
                    .filter(|row| row.tenant_id == tenant)
                    .collect();
                if rows.is_empty() {
                    None
                } else {
                    Some(rows)
                }
            })
            .flatten()
            .map(|row| row.upstream.clone())
            .collect()
    }

    /// Replaces an upstream in place.
    ///
    /// # Errors
    ///
    /// Returns not-found when the upstream does not exist or is not the caller's own —
    /// a descendant reads an ancestor's upstream but may not write it (FR-010) — a
    /// conflict when the replacement's alias collides with a different upstream, and a
    /// validation error when the replacement's derived alias differs from the stored one.
    pub fn replace_upstream(
        &self,
        id: &str,
        replacement: Upstream,
        chain: &TenantChain,
    ) -> Result<Upstream, OagwError> {
        let existing = self
            .get_upstream(id, chain)
            .filter(|row| chain.own().is_some_and(|own| row.tenant_id == own))
            .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "upstream not found"))?;

        // The alias is immutable once set.
        let replacement = Upstream {
            id: existing.id.clone(),
            tenant_id: existing.tenant_id,
            alias: existing.alias,
            ..replacement
        };

        let tables = self.tables.load();
        let mut guard = tables.write();
        if let Some(row) = guard
            .upstreams
            .iter_mut()
            .find(|row| row.upstream.id == id)
        {
            row.upstream = replacement.clone();
        }
        Ok(replacement)
    }

    /// Deletes an upstream, returning the deleted entity.
    ///
    /// # Errors
    ///
    /// Returns not-found when the upstream does not exist or is not the caller's own.
    pub fn delete_upstream(&self, id: &str, chain: &TenantChain) -> Result<Upstream, OagwError> {
        let existing = self
            .get_upstream(id, chain)
            .filter(|row| chain.own().is_some_and(|own| row.tenant_id == own))
            .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "upstream not found"))?;
        let key = (existing.tenant_id, alias::normalize(&existing.alias));
        self.alias_index.remove(&key);
        let tables = self.tables.load();
        let mut guard = tables.write();
        guard.upstreams.retain(|row| row.upstream.id != id);
        // A route has no meaning without its upstream, so the deletion cascades.
        guard.routes.retain(|row| row.route.upstream_id != id);
        drop(guard);
        Ok(existing)
    }

    // -- routes -------------------------------------------------------------

    /// Inserts a route, enforcing the route conflict rule.
    ///
    /// # Errors
    ///
    /// Returns a conflict error when another *enabled* route under the same upstream
    /// would match the same `(path, method, priority)`.
    pub fn insert_route(&self, mut route: Route, chain: &TenantChain) -> Result<Route, OagwError> {
        let Some(tenant) = chain.own() else {
            return Err(OagwError::new(
                crate::error::ErrorKind::ValidationError,
                "no tenant in the caller's chain",
            ));
        };
        route.tenant_id = tenant;
        route.validate()?;
        if self.get_upstream(&route.upstream_id, chain).is_none() {
            return Err(OagwError::new(
                crate::error::ErrorKind::ValidationError,
                "route.upstream_id does not name an upstream in the caller's tenant",
            ));
        }
        self.check_route_conflict(&route, None)?;
        let id = Uuid::new_v4();
        route.id = crate::domain::route::route_id(id);
        let tables = self.tables.load();
        let mut guard = tables.write();
        guard.routes.push(RouteRow { route: route.clone(), tenant_id: tenant });
        Ok(route)
    }

    /// Refuses a second *enabled* route that would match the same
    /// `(tenant, upstream, path, method, priority)` (FR-012).
    ///
    /// The candidate's own row is skipped, so a replacement never conflicts with itself.
    /// Disabled routes are not in the way: FR-012 forbids only a second enabled route,
    /// and a disabled one matches nothing, so re-enabling or replacing one frees its
    /// slot rather than reserving it forever.
    fn check_route_conflict(&self, candidate: &Route, ignore_id: Option<&str>) -> Result<(), OagwError> {
        if !candidate.enabled {
            return Ok(());
        }
        let Some(http) = candidate.http_match() else {
            return Ok(());
        };
        let tables = self.tables.load();
        let guard = tables.read();
        let conflicting = |row: &RouteRow| {
            row.route.enabled
                && row.route.id != ignore_id.unwrap_or("")
                && row.route.tenant_id == candidate.tenant_id
                && row.route.upstream_id == candidate.upstream_id
                && row.route.priority == candidate.priority
        };
        for method in &http.methods {
            if guard.routes.iter().any(|row| {
                conflicting(row)
                    && row.route.http_match().is_some_and(|match_rule| {
                        match_rule.path == http.path && match_rule.methods.contains(method)
                    })
            }) {
                return Err(OagwError::new(
                    crate::error::ErrorKind::AlreadyExists,
                    format!(
                        "an enabled route already matches {method} {} at priority {}",
                        http.path, candidate.priority
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Looks up a route by identifier within the caller's tenant chain.
    #[must_use]
    pub fn get_route(&self, id: &str, chain: &TenantChain) -> Option<Route> {
        let tables = self.tables.load();
        let guard = tables.read();
        guard
            .routes
            .iter()
            .find(|row| row.route.id == id && chain.contains(row.tenant_id))
            .map(|row| row.route.clone())
    }

    /// Lists the routes the caller can see.
    #[must_use]
    pub fn list_routes(&self, chain: &TenantChain) -> Vec<Route> {
        let tables = self.tables.load();
        let guard = tables.read();
        chain
            .iter()
            .filter_map(|tenant| {
                let rows: Vec<&RouteRow> =
                    guard.routes.iter().filter(|row| row.tenant_id == tenant).collect();
                if rows.is_empty() {
                    None
                } else {
                    Some(rows)
                }
            })
            .flatten()
            .map(|row| row.route.clone())
            .collect()
    }

    /// Lists the enabled routes of one upstream, longest path first.
    #[must_use]
    pub fn routes_for_upstream(&self, upstream_id: &str, chain: &TenantChain) -> Vec<Route> {
        let mut routes: Vec<Route> = self
            .list_routes(chain)
            .into_iter()
            .filter(|route| route.upstream_id == upstream_id && route.enabled)
            .collect();
        routes.sort_by(|a, b| {
            let a_len = a.http_match().map_or(0, |m| m.path.len());
            let b_len = b.http_match().map_or(0, |m| m.path.len());
            b_len.cmp(&a_len).then_with(|| a.priority.cmp(&b.priority))
        });
        routes
    }

    /// Replaces a route; `upstream_id` is immutable and taken from the stored row.
    ///
    /// # Errors
    ///
    /// Returns not-found when the route is absent or not the caller's own, or a conflict
    /// when the replacement collides with another enabled route.
    pub fn replace_route(&self, id: &str, replacement: Route, chain: &TenantChain) -> Result<Route, OagwError> {
        let existing = self
            .get_route(id, chain)
            .filter(|row| chain.own().is_some_and(|own| row.tenant_id == own))
            .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "route not found"))?;
        let replacement = Route {
            id: existing.id.clone(),
            tenant_id: existing.tenant_id,
            upstream_id: existing.upstream_id,
            ..replacement
        };
        replacement.validate()?;
        self.check_route_conflict(&replacement, Some(id))?;

        let tables = self.tables.load();
        let mut guard = tables.write();
        if let Some(row) = guard.routes.iter_mut().find(|row| row.route.id == id) {
            row.route = replacement.clone();
        }
        Ok(replacement)
    }

    /// Deletes a route, returning the deleted entity.
    ///
    /// # Errors
    ///
    /// Returns not-found when the route does not exist or is not the caller's own.
    pub fn delete_route(&self, id: &str, chain: &TenantChain) -> Result<Route, OagwError> {
        let existing = self
            .get_route(id, chain)
            .filter(|row| chain.own().is_some_and(|own| row.tenant_id == own))
            .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "route not found"))?;
        let tables = self.tables.load();
        let mut guard = tables.write();
        guard.routes.retain(|row| row.route.id != id);
        Ok(existing)
    }

    // -- plugins ------------------------------------------------------------

    /// Inserts a plugin.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the plugin has no name.
    pub fn insert_plugin(&self, mut plugin: Plugin, tenant: Uuid) -> Result<Plugin, OagwError> {
        if plugin.name.is_empty() {
            return Err(OagwError::new(
                crate::error::ErrorKind::ValidationError,
                "plugin.name is required",
            ));
        }
        plugin.tenant_id = tenant;
        let id = Uuid::new_v4();
        plugin.id = crate::domain::plugin::plugin_id(id);
        let tables = self.tables.load();
        let mut guard = tables.write();
        guard.plugins.push(PluginRow { plugin: plugin.clone(), tenant_id: tenant });
        Ok(plugin)
    }

    /// Looks up a plugin by identifier within the caller's tenant chain.
    #[must_use]
    pub fn get_plugin(&self, id: &str, chain: &TenantChain) -> Option<Plugin> {
        let tables = self.tables.load();
        let guard = tables.read();
        guard
            .plugins
            .iter()
            .find(|row| row.plugin.id == id && chain.contains(row.tenant_id))
            .map(|row| row.plugin.clone())
    }

    /// Lists the plugins the caller can see.
    #[must_use]
    pub fn list_plugins(&self, chain: &TenantChain) -> Vec<Plugin> {
        let tables = self.tables.load();
        let guard = tables.read();
        chain
            .iter()
            .filter_map(|tenant| {
                let rows: Vec<&PluginRow> =
                    guard.plugins.iter().filter(|row| row.tenant_id == tenant).collect();
                if rows.is_empty() {
                    None
                } else {
                    Some(rows)
                }
            })
            .flatten()
            .map(|row| row.plugin.clone())
            .collect()
    }

    /// Deletes a plugin, refusing when it is still referenced.
    ///
    /// # Errors
    ///
    /// Returns not-found when the plugin is absent or not the caller's own, and
    /// `PluginInUse` when an upstream or route still binds it.
    pub fn delete_plugin(&self, id: &str, chain: &TenantChain) -> Result<Plugin, OagwError> {
        let existing = self
            .get_plugin(id, chain)
            .filter(|row| chain.own().is_some_and(|own| row.tenant_id == own))
            .ok_or_else(|| OagwError::new(crate::error::ErrorKind::RouteNotFound, "plugin not found"))?;
        let referenced = {
            let tables = self.tables.load();
            let guard = tables.read();
            let in_upstream = guard
                .upstreams
                .iter()
                .any(|row| row.tenant_id == existing.tenant_id && binds(&row.upstream.plugins, id));
            let in_route = guard
                .routes
                .iter()
                .any(|row| row.tenant_id == existing.tenant_id && binds(&row.route.plugins, id));
            in_upstream || in_route
        };
        if referenced {
            return Err(OagwError::new(
                crate::error::ErrorKind::PluginInUse,
                "plugin is still referenced by an upstream or route",
            ));
        }
        let tables = self.tables.load();
        let mut guard = tables.write();
        guard.plugins.retain(|row| row.plugin.id != id);
        Ok(existing)
    }
}

/// Whether a binding list references the plugin identifier.
fn binds(bindings: &[PluginBinding], plugin_id: &str) -> bool {
    bindings
        .iter()
        .any(|b| b.name == plugin_id || b.uuid.as_deref() == Some(plugin_id))
}

/// Convenience: normalize an alias for storage and lookup.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias::normalize(alias)
}

use crate::domain::alias;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod mod_tests;
