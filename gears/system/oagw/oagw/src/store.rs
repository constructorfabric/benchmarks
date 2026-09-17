// Created: 2026-09-03 by Constructor Tech
//! In-memory control-plane store.
//!
//! State is process-local (see `DESIGN.md` §3.2): the control plane is the
//! single writer and the data plane reads through the same maps, so a
//! configuration change is visible to the next proxy request.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};
use crate::model::{PluginRecord, Route, Upstream};

/// A round-robin cursor for an upstream endpoint pool.
struct PoolCursor(AtomicUsize);

impl PoolCursor {
    fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    fn next(&self, modulo: usize) -> usize {
        if modulo <= 1 {
            0
        } else {
            self.0.fetch_add(1, Ordering::Relaxed) % modulo
        }
    }
}

/// Process-local repository of upstreams, routes and plugins.
#[derive(Default)]
pub struct OagwStore {
    upstreams: DashMap<(Uuid, Uuid), Arc<Upstream>>,
    aliases: DashMap<(Uuid, String), Uuid>,
    routes: DashMap<(Uuid, Uuid), Arc<Route>>,
    plugins: DashMap<(Uuid, Uuid), Arc<PluginRecord>>,
    cursors: DashMap<Uuid, PoolCursor>,
}

impl OagwStore {
    /// Registers a new upstream, rejecting a duplicate alias per tenant.
    ///
    /// # Errors
    /// Returns 409 `AliasConflict` when the alias is already registered for
    /// the tenant.
    pub fn insert_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        let key = (upstream.tenant_id, upstream.alias.clone());
        if self.aliases.contains_key(&key) {
            return Err(OagwError::new(
                ErrorKind::AliasConflict,
                format!("alias '{}' is already registered for this tenant", upstream.alias),
            ));
        }
        let id = upstream.id;
        let stored = Arc::new(upstream);
        self.aliases.insert(key, id);
        self.upstreams.insert((stored.tenant_id, id), Arc::clone(&stored));
        self.cursors.insert(id, PoolCursor::new());
        Ok(stored)
    }

    /// Replaces an existing upstream, keeping its identity and alias.
    ///
    /// # Errors
    /// Returns 404 when the upstream does not exist for the tenant.
    pub fn replace_upstream(&self, upstream: Upstream) -> Result<Arc<Upstream>, OagwError> {
        let key = (upstream.tenant_id, upstream.id);
        // The alias check reads through a shard guard; it must be released
        // before the map is written again or the shard write would deadlock.
        let alias = {
            let existing = self
                .upstreams
                .get(&key)
                .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "upstream not found"))?;
            existing.alias.clone()
        };
        if alias != upstream.alias {
            return Err(OagwError::new(
                ErrorKind::AliasConflict,
                "alias is immutable and cannot be changed by a replace",
            ));
        }
        let stored = Arc::new(upstream);
        self.upstreams.insert(key, Arc::clone(&stored));
        Ok(stored)
    }

    /// Fetches an upstream owned by `tenant_id`.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Upstream>> {
        self.upstreams.get(&(tenant_id, id)).map(|e| Arc::clone(&e))
    }

    /// Fetches an upstream by its routing alias.
    #[must_use]
    pub fn get_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        let id = self
            .aliases
            .get(&(tenant_id, alias.to_owned()))
            .map(|e| *e.value())?;
        self.get_upstream(tenant_id, id)
    }

    /// Lists the upstreams owned by `tenant_id`, ordered by alias.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        let mut items: Vec<Arc<Upstream>> = self
            .upstreams
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        items.sort_by(|a, b| a.alias.cmp(&b.alias));
        items
    }

    /// Deletes an upstream.
    ///
    /// # Errors
    /// Returns 404 when the upstream does not exist and 409 `RouteConflict`
    /// when routes still reference it.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Upstream>, OagwError> {
        let existing = self
            .get_upstream(tenant_id, id)
            .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "upstream not found"))?;
        if self
            .routes
            .iter()
            .any(|entry| entry.key().0 == tenant_id && entry.value().upstream_id == id)
        {
            return Err(OagwError::new(
                ErrorKind::RouteConflict,
                "upstream is still referenced by one or more routes",
            ));
        }
        self.upstreams.remove(&(tenant_id, id));
        self.aliases.remove(&(tenant_id, existing.alias.clone()));
        self.cursors.remove(&id);
        Ok(existing)
    }

    /// Picks the next endpoint of a pool in round-robin order.
    #[must_use]
    pub fn next_endpoint_index(&self, upstream_id: Uuid, pool_size: usize) -> usize {
        let cursor = self
            .cursors
            .entry(upstream_id)
            .or_insert_with(PoolCursor::new);
        cursor.next(pool_size)
    }

    /// Inserts a route.
    ///
    /// # Errors
    /// Returns 409 `RouteConflict` when an identical match rule already
    /// exists for the same upstream.
    pub fn insert_route(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        let duplicates = self.routes_of(route.tenant_id, route.upstream_id);
        if duplicates
            .iter()
            .any(|existing| existing.r#match == route.r#match)
        {
            return Err(OagwError::new(
                ErrorKind::RouteConflict,
                "a route with the same match rules already exists for this upstream",
            ));
        }
        let stored = Arc::new(route);
        self.routes.insert((stored.tenant_id, stored.id), Arc::clone(&stored));
        Ok(stored)
    }

    /// Replaces an existing route.
    ///
    /// # Errors
    /// Returns 404 when the route does not exist, and 409 `RouteConflict`
    /// when the new match rules collide with another route.
    pub fn replace_route(&self, route: Route) -> Result<Arc<Route>, OagwError> {
        let key = (route.tenant_id, route.id);
        // Release the shard guard before writing the same shard again.
        let upstream_id = {
            let existing = self
                .routes
                .get(&key)
                .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "route not found"))?;
            existing.upstream_id
        };
        if upstream_id != route.upstream_id {
            return Err(OagwError::new(
                ErrorKind::RouteConflict,
                "upstream_id is immutable on a route; delete and re-create the route",
            ));
        }
        let stored = Arc::new(route);
        self.routes.insert(key, Arc::clone(&stored));
        Ok(stored)
    }

    /// Fetches a route owned by `tenant_id`.
    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<Route>> {
        self.routes.get(&(tenant_id, id)).map(|e| Arc::clone(&e))
    }

    /// Lists the routes owned by `tenant_id`, ordered by id.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        let mut items: Vec<Arc<Route>> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        items.sort_by_key(|a| a.id);
        items
    }

    /// Lists the routes of a single upstream.
    #[must_use]
    pub fn routes_of(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        let mut items: Vec<Arc<Route>> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant_id && entry.value().upstream_id == upstream_id)
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        items.sort_by_key(|a| a.id);
        items
    }

    /// Deletes a route.
    ///
    /// # Errors
    /// Returns 404 when the route does not exist.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Route>, OagwError> {
        self.routes
            .remove(&(tenant_id, id))
            .map(|(_, value)| value)
            .ok_or_else(|| OagwError::new(ErrorKind::ResourceNotFound, "route not found"))
    }

    /// Inserts a custom plugin definition.
    ///
    /// # Errors
    /// Returns 409 `AliasConflict` when the name is taken for the tenant.
    pub fn insert_plugin(&self, plugin: PluginRecord) -> Result<Arc<PluginRecord>, OagwError> {
        if self
            .list_plugins(plugin.tenant_id)
            .iter()
            .any(|existing| existing.name == plugin.name)
        {
            return Err(OagwError::new(
                ErrorKind::AliasConflict,
                format!("plugin name '{}' is already registered for this tenant", plugin.name),
            ));
        }
        let stored = Arc::new(plugin);
        self.plugins.insert((stored.tenant_id, stored.id), Arc::clone(&stored));
        Ok(stored)
    }

    /// Fetches a custom plugin owned by `tenant_id`.
    #[must_use]
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<Arc<PluginRecord>> {
        self.plugins.get(&(tenant_id, id)).map(|e| Arc::clone(&e))
    }

    /// Lists the custom plugins owned by `tenant_id`, ordered by name.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Arc<PluginRecord>> {
        let mut items: Vec<Arc<PluginRecord>> = self
            .plugins
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| Arc::clone(entry.value()))
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name));
        items
    }

    /// Deletes a custom plugin, refusing while it is still referenced.
    ///
    /// # Errors
    /// Returns 404 when the plugin does not exist and 409 `PluginInUse` with
    /// the referencing upstream and route identifiers otherwise.
    pub fn delete_plugin(
        &self,
        tenant_id: Uuid,
        id: Uuid,
    ) -> Result<Arc<PluginRecord>, OagwError> {
        let existing = self
            .get_plugin(tenant_id, id)
            .ok_or_else(|| OagwError::new(ErrorKind::PluginNotFound, "plugin not found"))?;
        let identifier = existing.id.to_string();
        let (upstream_refs, route_refs) = self.plugin_references(tenant_id, &identifier);
        if !upstream_refs.is_empty() || !route_refs.is_empty() {
            return Err(crate::error::plugin_in_use(&upstream_refs, &route_refs));
        }
        self.plugins.remove(&(tenant_id, id));
        Ok(existing)
    }

    /// The upstreams and routes referencing a plugin identifier.
    #[must_use]
    pub fn plugin_references(
        &self,
        tenant_id: Uuid,
        plugin_identifier: &str,
    ) -> (Vec<String>, Vec<String>) {
        let upstream_refs: Vec<String> = self
            .list_upstreams(tenant_id)
            .iter()
            .filter(|upstream| {
                crate::plugins::references_of_upstream(upstream)
                    .iter()
                    .any(|item| item.plugin_ref() == plugin_identifier)
            })
            .map(|upstream| crate::gts::upstream_id(upstream.id))
            .collect();
        let route_refs: Vec<String> = self
            .list_routes(tenant_id)
            .iter()
            .filter(|route| {
                crate::plugins::references_of_route(route)
                    .iter()
                    .any(|item| item.plugin_ref() == plugin_identifier)
            })
            .map(|route| crate::gts::route_id(route.id))
            .collect();
        (upstream_refs, route_refs)
    }

    /// Marks a plugin as used by the data plane.
    pub fn touch_plugin(&self, tenant_id: Uuid, id: Uuid, at: i64) {
        if let Some(mut entry) = self.plugins.get_mut(&(tenant_id, id)) {
            let mut updated = (**entry).clone();
            updated.last_used_at = Some(at);
            updated.gc_eligible_at = None;
            *entry = Arc::new(updated);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::model::{
        Endpoint, EndpointScheme, HttpMethod, HttpMatch, PathSuffixMode, RouteMatch, ServerConfig,
    };

    fn upstream(alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.example.com".to_owned(),
                    port: Some(443),
                }],
            },
            protocol: crate::gts::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn route(upstream_id: Uuid) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id,
            enabled: true,
            tags: Vec::new(),
            r#match: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: "/v1".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn alias_conflict_is_detected_per_tenant() {
        let store = OagwStore::default();
        let tenant = Uuid::new_v4();
        let mut first = upstream("api.example.com");
        first.tenant_id = tenant;
        store.insert_upstream(first).expect("first insert");
        let mut second = upstream("api.example.com");
        second.tenant_id = tenant;
        assert!(store.insert_upstream(second).is_err());
        let other_tenant = upstream("api.example.com");
        assert!(store.insert_upstream(other_tenant).is_ok());
        assert_eq!(store.list_upstreams(tenant).len(), 1);
    }

    #[test]
    fn route_conflict_and_upstream_delete_protection() {
        let store = OagwStore::default();
        let mut created = upstream("api.example.com");
        created.id = Uuid::new_v4();
        store.insert_upstream(created.clone()).expect("insert");
        let mut route = route(created.id);
        route.tenant_id = created.tenant_id;
        let route_id = route.id;
        store.insert_route(route.clone()).expect("insert route");
        assert!(store.insert_route(route).is_err());
        assert!(store.delete_upstream(created.tenant_id, created.id).is_err());
        assert!(store.delete_route(created.tenant_id, route_id).is_ok());
        assert!(store.delete_upstream(created.tenant_id, created.id).is_ok());
        assert!(store.get_route(created.tenant_id, route_id).is_none());
    }

    #[test]
    fn plugin_deletion_is_refused_while_referenced() {
        let store = OagwStore::default();
        let tenant = Uuid::new_v4();
        let mut created = upstream("api.example.com");
        created.tenant_id = tenant;
        created.id = Uuid::new_v4();
        store.insert_upstream(created.clone()).expect("insert");
        let record = PluginRecord {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            name: "my-guard".to_owned(),
            plugin_type: crate::model::PluginType::Guard,
            config: None,
            created_at: 0,
            last_used_at: None,
            gc_eligible_at: None,
        };
        let plugin_id = record.id;
        store.insert_plugin(record).expect("insert plugin");
        let mut bound = upstream("other.example.com");
        bound.tenant_id = tenant;
        bound.id = Uuid::new_v4();
        bound.plugins = Some(crate::model::PluginsConfig {
            sharing: crate::model::SharingMode::Private,
            items: vec![crate::model::PluginItem::Ref(plugin_id.to_string())],
        });
        store.insert_upstream(bound).expect("insert bound");
        let error = store
            .delete_plugin(tenant, plugin_id)
            .expect_err("plugin is still referenced");
        assert_eq!(error.kind(), crate::error::ErrorKind::PluginInUse);
        let (upstream_refs, route_refs) = store.plugin_references(tenant, &plugin_id.to_string());
        assert_eq!(upstream_refs.len(), 1);
        assert!(route_refs.is_empty());
    }

    #[test]
    fn replace_operations_complete_and_enforce_immutability() {
        let store = OagwStore::default();
        let mut created = upstream("api.example.com");
        created.id = Uuid::new_v4();
        store.insert_upstream(created.clone()).expect("insert");
        created.tags = vec!["rotated".to_owned()];
        let replaced = store.replace_upstream(created.clone()).expect("replace");
        assert_eq!(replaced.tags, vec!["rotated".to_owned()]);

        let mut renamed = created.clone();
        renamed.alias = "other.example.com".to_owned();
        let error = store.replace_upstream(renamed).expect_err("alias is immutable");
        assert_eq!(error.kind(), crate::error::ErrorKind::AliasConflict);

        let mut route = route(created.id);
        route.tenant_id = created.tenant_id;
        route.id = Uuid::new_v4();
        store.insert_route(route.clone()).expect("insert");
        route.r#match.http.as_mut().expect("http").path = "/v2".to_owned();
        let replaced = store.replace_route(route.clone()).expect("replace route");
        assert_eq!(replaced.r#match.http.as_ref().expect("http").path, "/v2");

        let mut repointed = route.clone();
        repointed.upstream_id = Uuid::new_v4();
        let error = store.replace_route(repointed).expect_err("upstream binding is immutable");
        assert_eq!(error.kind(), crate::error::ErrorKind::RouteConflict);
    }

    #[test]
    fn round_robin_walks_the_pool() {
        let store = OagwStore::default();
        let mut created = upstream("api.example.com");
        created.server.endpoints.push(Endpoint {
            scheme: EndpointScheme::Http,
            host: "b.example.com".to_owned(),
            port: Some(80),
        });
        let id = created.id;
        store.insert_upstream(created).expect("insert");
        let picks: Vec<usize> = (0..4)
            .map(|_| store.next_endpoint_index(id, 2))
            .collect();
        assert_eq!(picks, vec![0, 1, 0, 1]);
        assert_eq!(store.next_endpoint_index(id, 1), 0);
    }
}
