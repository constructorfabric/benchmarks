//! In-process control-plane store.
//!
//! The upstream crate does not depend on a database stack (`toolkit-db` /
//! `sea-orm` are not declared in this crate's manifest), so the control plane
//! persists configuration in a mutex-protected map. The repository traits in
//! [`crate::domain::repo`] keep this swappable; the externally observable REST
//! behaviour (paths, payloads, status codes) is unchanged.
//!
//! Consistency: every mutation takes a single write lock, so the
//! `UNIQUE (tenant_id, alias)` and route match-uniqueness invariants are
//! enforced atomically.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::domain::error::{DomainError, DomainResult, ErrorKind};
use crate::domain::model::{HttpMatch, Plugin, Route, Upstream};
use crate::domain::repo::{ListFilter, PluginRepository, RouteRepository, UpstreamRepository};

type Resource<T> = RwLock<HashMap<uuid::Uuid, T>>;

/// Conflict (409) with a stable reason code.
fn conflict(detail: impl Into<String>) -> DomainError {
    DomainError::new(ErrorKind::Conflict, detail)
}

/// Not found (404).
fn not_found(kind: &str, id: uuid::Uuid) -> DomainError {
    DomainError::new(ErrorKind::RouteNotFound, format!("{kind} '{id}' not found"))
}

/// Shared handle to the control-plane store.
#[derive(Clone, Default)]
pub struct InMemoryStore {
    upstreams: Arc<Resource<Upstream>>,
    routes: Arc<Resource<Route>>,
    plugins: Arc<Resource<Plugin>>,
}

impl InMemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when any upstream or route still references `plugin_id`.
    #[must_use]
    pub fn plugin_is_referenced(&self, plugin_id: &uuid::Uuid) -> bool {
        let upstreams = self.upstreams.read();
        if upstreams.values().any(|u| {
            u.plugins
                .items
                .iter()
                .any(|binding| binding.id == plugin_id.to_string())
        }) {
            return true;
        }
        let routes = self.routes.read();
        routes.values().any(|r| {
            r.plugins
                .items
                .iter()
                .any(|binding| binding.id == plugin_id.to_string())
        })
    }

    /// `true` when any route references `upstream_id`.
    #[must_use]
    pub fn upstream_is_referenced(&self, upstream_id: &uuid::Uuid) -> bool {
        self.routes
            .read()
            .values()
            .any(|r| &r.upstream_id == upstream_id)
    }

    /// `true` when another upstream of the same tenant already uses `alias`.
    fn alias_taken(&self, tenant_id: &uuid::Uuid, alias: &str, except: &uuid::Uuid) -> bool {
        self.upstreams
            .read()
            .values()
            .any(|u| &u.tenant_id == tenant_id && u.alias == alias && &u.id != except)
    }

    /// `true` when a sibling route of the upstream declares an identical match.
    fn match_taken(
        &self,
        upstream_id: &uuid::Uuid,
        route_match: &crate::domain::model::RouteMatch,
        own_id: &uuid::Uuid,
    ) -> bool {
        self.routes.read().values().any(|r| {
            &r.upstream_id == upstream_id && &r.id != own_id && r.route_match == *route_match
        })
    }
}

/// `true` when another upstream of the same tenant already uses `alias`.
fn alias_taken_in(
    rows: &HashMap<uuid::Uuid, Upstream>,
    tenant_id: &uuid::Uuid,
    alias: &str,
    except: &uuid::Uuid,
) -> bool {
    rows.values()
        .any(|u| &u.tenant_id == tenant_id && u.alias == alias && &u.id != except)
}

/// `true` when a sibling route of the upstream declares an identical match.
fn match_taken_in(
    rows: &HashMap<uuid::Uuid, Route>,
    upstream_id: &uuid::Uuid,
    route_match: &crate::domain::model::RouteMatch,
    own_id: &uuid::Uuid,
) -> bool {
    rows.values()
        .any(|r| &r.upstream_id == upstream_id && &r.id != own_id && r.route_match == *route_match)
}

fn sorted_upstreams(rows: &HashMap<uuid::Uuid, Upstream>) -> Vec<Upstream> {
    let mut out: Vec<Upstream> = rows.values().cloned().collect();
    out.sort_by_key(|a| a.id);
    out
}

fn sorted_routes(rows: &HashMap<uuid::Uuid, Route>) -> Vec<Route> {
    let mut out: Vec<Route> = rows.values().cloned().collect();
    out.sort_by_key(|a| a.id);
    out
}

fn sorted_plugins(rows: &HashMap<uuid::Uuid, Plugin>) -> Vec<Plugin> {
    let mut out: Vec<Plugin> = rows.values().cloned().collect();
    out.sort_by_key(|a| a.id);
    out
}

#[async_trait]
impl UpstreamRepository for InMemoryStore {
    async fn insert(&self, upstream: Upstream) -> DomainResult<Upstream> {
        let mut upstream = upstream;
        if self.alias_taken(&upstream.tenant_id, &upstream.alias, &upstream.id) {
            return Err(conflict(format!(
                "an upstream with alias '{}' already exists in this tenant",
                upstream.alias
            )));
        }
        let mut guard = self.upstreams.write();
        upstream.created_at = crate::domain::model::now_millis();
        upstream.updated_at = upstream.created_at;
        guard.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn find_by_id(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Option<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .cloned())
    }

    async fn find_by_alias(
        &self,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> DomainResult<Option<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .values()
            .find(|u| u.tenant_id == tenant_id && u.alias == alias)
            .cloned())
    }

    async fn list(
        &self,
        tenant_id: uuid::Uuid,
        filter: &ListFilter,
    ) -> DomainResult<Vec<Upstream>> {
        let rows = sorted_upstreams(&self.upstreams.read());
        let mut out: Vec<Upstream> = rows
            .into_iter()
            .filter(|u| u.tenant_id == tenant_id)
            .filter(|u| filter.alias.as_ref().is_none_or(|a| *a == u.alias))
            .collect();
        if let Some(top) = filter.top {
            let skip = filter.skip.unwrap_or(0);
            out = out.into_iter().skip(skip).take(top).collect();
        }
        Ok(out)
    }

    async fn update(&self, upstream: Upstream) -> DomainResult<Upstream> {
        let mut upstream = upstream;
        let mut guard = self.upstreams.write();
        match guard.get(&upstream.id) {
            Some(existing) if existing.tenant_id == upstream.tenant_id => {
                upstream.created_at = existing.created_at;
            }
            _ => {
                return Err(DomainError::new(
                    ErrorKind::RouteNotFound,
                    format!("upstream '{}' not found", upstream.id),
                ));
            }
        }
        if alias_taken_in(&guard, &upstream.tenant_id, &upstream.alias, &upstream.id) {
            return Err(conflict(format!(
                "an upstream with alias '{}' already exists in this tenant",
                upstream.alias
            )));
        }
        upstream.updated_at = crate::domain::model::now_millis();
        guard.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()> {
        let mut guard = self.upstreams.write();
        match guard.get(&id) {
            Some(existing) if existing.tenant_id == tenant_id => {
                if self.upstream_is_referenced(&id) {
                    return Err(DomainError::new(
                        ErrorKind::Conflict,
                        "upstream is still referenced by one or more routes",
                    ));
                }
                guard.remove(&id);
                Ok(())
            }
            _ => Err(not_found("upstream", id)),
        }
    }
}

#[async_trait]
impl RouteRepository for InMemoryStore {
    async fn insert(&self, route: Route) -> DomainResult<Route> {
        let mut route = route;
        if self.match_taken(&route.upstream_id, &route.route_match, &route.id) {
            return Err(conflict(
                "an identical match rule already exists for this upstream",
            ));
        }
        let mut guard = self.routes.write();
        route.created_at = crate::domain::model::now_millis();
        route.updated_at = route.created_at;
        guard.insert(route.id, route.clone());
        Ok(route)
    }

    async fn find_by_id(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Option<Route>> {
        Ok(self
            .routes
            .read()
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .cloned())
    }

    async fn list(&self, tenant_id: uuid::Uuid, filter: &ListFilter) -> DomainResult<Vec<Route>> {
        let rows = sorted_routes(&self.routes.read());
        let mut out: Vec<Route> = rows
            .into_iter()
            .filter(|r| r.tenant_id == tenant_id)
            .filter(|r| filter.upstream_id.is_none_or(|id| id == r.upstream_id))
            .collect();
        if let Some(top) = filter.top {
            let skip = filter.skip.unwrap_or(0);
            out = out.into_iter().skip(skip).take(top).collect();
        }
        Ok(out)
    }

    async fn list_by_tenants(
        &self,
        tenant_ids: &[uuid::Uuid],
        filter: &ListFilter,
    ) -> DomainResult<Vec<Route>> {
        let rows = sorted_routes(&self.routes.read());
        let mut out: Vec<Route> = rows
            .into_iter()
            .filter(|r| tenant_ids.contains(&r.tenant_id))
            .filter(|r| filter.upstream_id.is_none_or(|id| id == r.upstream_id))
            .collect();
        if let Some(top) = filter.top {
            let skip = filter.skip.unwrap_or(0);
            out = out.into_iter().skip(skip).take(top).collect();
        }
        Ok(out)
    }

    async fn update(&self, route: Route) -> DomainResult<Route> {
        let mut route = route;
        let mut guard = self.routes.write();
        match guard.get(&route.id) {
            Some(existing) if existing.tenant_id == route.tenant_id => {
                route.created_at = existing.created_at;
                route.upstream_id = existing.upstream_id;
            }
            _ => {
                return Err(DomainError::new(
                    ErrorKind::RouteNotFound,
                    format!("route '{}' not found", route.id),
                ));
            }
        }
        if match_taken_in(&guard, &route.upstream_id, &route.route_match, &route.id) {
            return Err(conflict(
                "an identical match rule already exists for this upstream",
            ));
        }
        route.updated_at = crate::domain::model::now_millis();
        guard.insert(route.id, route.clone());
        Ok(route)
    }

    async fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()> {
        let mut guard = self.routes.write();
        match guard.get(&id) {
            Some(existing) if existing.tenant_id == tenant_id => {
                guard.remove(&id);
                Ok(())
            }
            _ => Err(not_found("route", id)),
        }
    }
}

#[async_trait]
impl PluginRepository for InMemoryStore {
    async fn insert(&self, plugin: Plugin) -> DomainResult<Plugin> {
        let mut plugin = plugin;
        let mut guard = self.plugins.write();
        plugin.created_at = crate::domain::model::now_millis();
        guard.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    async fn find_by_id(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
    ) -> DomainResult<Option<Plugin>> {
        Ok(self
            .plugins
            .read()
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .cloned())
    }

    async fn list(&self, tenant_id: uuid::Uuid, filter: &ListFilter) -> DomainResult<Vec<Plugin>> {
        let rows = sorted_plugins(&self.plugins.read());
        let mut out: Vec<Plugin> = rows
            .into_iter()
            .filter(|p| p.tenant_id == tenant_id)
            .filter(|p| {
                filter
                    .plugin_type
                    .as_ref()
                    .is_none_or(|kind| p.plugin_type == *kind)
            })
            .collect();
        if let Some(top) = filter.top {
            let skip = filter.skip.unwrap_or(0);
            out = out.into_iter().skip(skip).take(top).collect();
        }
        Ok(out)
    }

    async fn delete(&self, tenant_id: uuid::Uuid, id: uuid::Uuid) -> DomainResult<()> {
        let mut guard = self.plugins.write();
        match guard.get(&id) {
            Some(existing) if existing.tenant_id == tenant_id => {
                if self.plugin_is_referenced(&id) {
                    return Err(DomainError::new(
                        ErrorKind::PluginInUse,
                        "plugin is still referenced by an upstream or route",
                    ));
                }
                guard.remove(&id);
                Ok(())
            }
            _ => Err(not_found("plugin", id)),
        }
    }

    async fn mark_gc_eligible(
        &self,
        tenant_id: uuid::Uuid,
        id: uuid::Uuid,
        at_millis: i64,
    ) -> DomainResult<()> {
        let mut guard = self.plugins.write();
        match guard.get_mut(&id) {
            Some(existing) if existing.tenant_id == tenant_id => {
                existing.gc_eligible_at = Some(at_millis);
                Ok(())
            }
            _ => Err(not_found("plugin", id)),
        }
    }
}

/// Public helper re-exported for the data plane: checks a route match against
/// an existing route without touching storage.
#[must_use]
pub fn http_match_signature(match_rule: &HttpMatch) -> String {
    let mut methods = match_rule.methods.clone();
    methods.sort();
    format!("{}|{}", methods.join(","), match_rule.path)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{CorsConfig, Endpoint, EndpointScheme, Protocol, ServerConfig};

    fn upstream(tenant: uuid::Uuid, alias: &str) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            alias: alias.to_owned(),
            tags: Default::default(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.example".to_owned(),
                    port: None,
                }],
            },
            protocol: Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: CorsConfig::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn alias_conflicts_are_rejected_with_409() {
        let store = InMemoryStore::new();
        let tenant = uuid::Uuid::new_v4();
        UpstreamRepository::insert(&store, upstream(tenant, "a.example"))
            .await
            .expect("first insert");
        let err = UpstreamRepository::insert(&store, upstream(tenant, "a.example"))
            .await
            .expect_err("conflict");
        assert_eq!(err.kind.status(), http::StatusCode::CONFLICT);
    }

    #[test]
    fn plugin_delete_is_blocked_while_referenced() {
        let store = InMemoryStore::new();
        let tenant = uuid::Uuid::new_v4();
        let mut up = upstream(tenant, "a.example");
        let plugin_id = uuid::Uuid::new_v4();
        up.plugins.items.push(crate::domain::model::PluginBinding {
            id: plugin_id.to_string(),
            sharing: None,
            config: Default::default(),
        });
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async {
            UpstreamRepository::insert(&store, up)
                .await
                .expect("insert upstream");
            let plugin = crate::domain::model::Plugin {
                id: plugin_id,
                tenant_id: tenant,
                name: "guard".to_owned(),
                plugin_type: "guard_plugin".to_owned(),
                source: String::new(),
                gc_eligible_at: None,
                created_at: 0,
            };
            PluginRepository::insert(&store, plugin)
                .await
                .expect("insert plugin");
            let err = PluginRepository::delete(&store, tenant, plugin_id)
                .await
                .expect_err("plugin must be in use");
            assert_eq!(err.kind, crate::domain::error::ErrorKind::PluginInUse);
            store
                .mark_gc_eligible(tenant, plugin_id, 1)
                .await
                .expect("mark gc");
            assert!(store.plugin_is_referenced(&plugin_id));
        });
    }
}
