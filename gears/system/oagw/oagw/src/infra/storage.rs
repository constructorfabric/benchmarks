//! In-process repository implementations.
//!
//! OAGW's configuration store is a set of concurrent maps guarded by the same
//! invariants the relational baseline in `DESIGN.md` § *Database Schemas &
//! Tables* states: `UNIQUE (tenant_id, alias)` on upstreams,
//! `UNIQUE (tenant_id, name)` on plugins, and cascade-on-delete from an
//! upstream to its routes. Multi-map updates are done under the entry lock of
//! the index that enforces the uniqueness constraint, so a create either
//! publishes both the row and its index entry or neither.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use uuid::Uuid;

use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::{PluginRecord, Route, Upstream};
use crate::domain::repo::{
    PluginRepository, PluginUsage, PluginUsageRepository, RouteRepository, UpstreamRepository,
};

/// Shared configuration store backing all three repositories.
#[derive(Debug, Default)]
pub struct MemoryStore {
    upstreams: DashMap<Uuid, Upstream>,
    /// `(tenant_id, alias)` → upstream id. Enforces alias uniqueness.
    alias_index: DashMap<(Uuid, String), Uuid>,
    routes: DashMap<Uuid, Route>,
    plugins: DashMap<Uuid, PluginRecord>,
    /// `(tenant_id, name)` → plugin id. Enforces plugin-name uniqueness.
    plugin_name_index: DashMap<(Uuid, String), Uuid>,
}

impl MemoryStore {
    /// A fresh, empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wall-clock seconds since the Unix epoch — the clock GC deadlines are
    /// expressed in.
    pub fn clock(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs())
    }

    /// Every plugin binding reference currently in use, paired with the
    /// resource that holds it. Used by the plugin-deletion check.
    #[must_use]
    pub fn plugin_references(&self, plugin_ref: &str) -> PluginReferences {
        let mut refs = PluginReferences::default();
        for entry in &self.upstreams {
            let upstream = entry.value();
            let auth_hit = upstream
                .auth
                .as_ref()
                .is_some_and(|auth| auth.plugin_type == plugin_ref);
            let chain_hit = upstream
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|i| i.plugin_ref == plugin_ref));
            if auth_hit || chain_hit {
                refs.upstreams
                    .push(crate::domain::gts_helpers::anonymous_id(
                        crate::domain::gts_helpers::UPSTREAM_TYPE,
                        upstream.id,
                    ));
            }
        }
        for entry in &self.routes {
            let route = entry.value();
            if route
                .plugins
                .as_ref()
                .is_some_and(|p| p.items.iter().any(|i| i.plugin_ref == plugin_ref))
            {
                refs.routes.push(crate::domain::gts_helpers::anonymous_id(
                    crate::domain::gts_helpers::ROUTE_TYPE,
                    route.id,
                ));
            }
        }
        refs
    }

    /// Delete plugin rows whose GC deadline has passed.
    pub fn collect_garbage(&self, now: u64) -> usize {
        let doomed: Vec<Uuid> = self
            .plugins
            .iter()
            .filter(|entry| entry.value().gc_eligible_at.is_some_and(|at| at <= now))
            .map(|entry| *entry.key())
            .collect();
        for id in &doomed {
            if let Some((_, plugin)) = self.plugins.remove(id) {
                self.plugin_name_index
                    .remove(&(plugin.tenant_id, plugin.name.clone()));
            }
        }
        doomed.len()
    }
}

/// Which resources still reference a plugin.
#[derive(Debug, Default)]
pub struct PluginReferences {
    /// GTS identifiers of referencing upstreams.
    pub upstreams: Vec<String>,
    /// GTS identifiers of referencing routes.
    pub routes: Vec<String>,
}

impl PluginReferences {
    /// Whether anything references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

/// [`UpstreamRepository`] over [`MemoryStore`].
#[derive(Debug, Clone)]
pub struct MemoryUpstreamRepo(Arc<MemoryStore>);

impl MemoryUpstreamRepo {
    /// Wrap a shared store.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamRepo {
    async fn create(&self, upstream: Upstream) -> OagwResult<Upstream> {
        let key = (upstream.tenant_id, upstream.alias.clone());
        match self.0.alias_index.entry(key) {
            Entry::Occupied(_) => Err(OagwError::new(
                ErrorKind::AliasConflict,
                format!(
                    "an upstream with alias {:?} already exists for this tenant",
                    upstream.alias
                ),
            )
            .with("alias", upstream.alias.clone())),
            Entry::Vacant(slot) => {
                slot.insert(upstream.id);
                self.0.upstreams.insert(upstream.id, upstream.clone());
                Ok(upstream)
            }
        }
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>> {
        Ok(self
            .0
            .upstreams
            .get(&id)
            .filter(|entry| entry.value().tenant_id == tenant_id)
            .map(|entry| entry.value().clone()))
    }

    async fn replace(&self, upstream: Upstream) -> OagwResult<Upstream> {
        match self.0.upstreams.entry(upstream.id) {
            Entry::Occupied(mut slot) if slot.get().tenant_id == upstream.tenant_id => {
                slot.insert(upstream.clone());
                Ok(upstream)
            }
            _ => Err(OagwError::not_found("upstream not found")),
        }
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool> {
        let Some((_, upstream)) = self
            .0
            .upstreams
            .remove_if(&id, |_, value| value.tenant_id == tenant_id)
        else {
            return Ok(false);
        };
        self.0
            .alias_index
            .remove(&(upstream.tenant_id, upstream.alias.clone()));
        // Cascade: routes are children of the upstream row.
        let doomed: Vec<Uuid> = self
            .0
            .routes
            .iter()
            .filter(|entry| entry.value().upstream_id == id)
            .map(|entry| *entry.key())
            .collect();
        for route_id in doomed {
            self.0.routes.remove(&route_id);
        }
        Ok(true)
    }

    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>> {
        let mut found: Vec<Upstream> = self
            .0
            .upstreams
            .iter()
            .filter(|entry| entry.value().tenant_id == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        found.sort_by_key(|u| u.seq);
        Ok(found)
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>> {
        let id = self
            .0
            .alias_index
            .get(&(tenant_id, alias.to_owned()))
            .map(|entry| *entry.value());
        Ok(id.and_then(|id| self.0.upstreams.get(&id).map(|e| e.value().clone())))
    }
}

/// [`RouteRepository`] over [`MemoryStore`].
#[derive(Debug, Clone)]
pub struct MemoryRouteRepo(Arc<MemoryStore>);

impl MemoryRouteRepo {
    /// Wrap a shared store.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteRepo {
    async fn create(&self, route: Route) -> OagwResult<Route> {
        self.0.routes.insert(route.id, route.clone());
        Ok(route)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>> {
        Ok(self
            .0
            .routes
            .get(&id)
            .filter(|entry| entry.value().tenant_id == tenant_id)
            .map(|entry| entry.value().clone()))
    }

    async fn replace(&self, route: Route) -> OagwResult<Route> {
        match self.0.routes.entry(route.id) {
            Entry::Occupied(mut slot) if slot.get().tenant_id == route.tenant_id => {
                slot.insert(route.clone());
                Ok(route)
            }
            _ => Err(OagwError::not_found("route not found")),
        }
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool> {
        Ok(self
            .0
            .routes
            .remove_if(&id, |_, value| value.tenant_id == tenant_id)
            .is_some())
    }

    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>> {
        let mut found: Vec<Route> = self
            .0
            .routes
            .iter()
            .filter(|entry| entry.value().tenant_id == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        found.sort_by_key(|r| r.seq);
        Ok(found)
    }

    async fn list_by_upstream(&self, upstream_id: Uuid) -> OagwResult<Vec<Route>> {
        let mut found: Vec<Route> = self
            .0
            .routes
            .iter()
            .filter(|entry| entry.value().upstream_id == upstream_id)
            .map(|entry| entry.value().clone())
            .collect();
        found.sort_by_key(|r| r.seq);
        Ok(found)
    }
}

/// [`PluginRepository`] over [`MemoryStore`].
#[derive(Debug, Clone)]
pub struct MemoryPluginRepo(Arc<MemoryStore>);

impl MemoryPluginRepo {
    /// Wrap a shared store.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginRepo {
    async fn create(&self, plugin: PluginRecord) -> OagwResult<PluginRecord> {
        let key = (plugin.tenant_id, plugin.name.clone());
        match self.0.plugin_name_index.entry(key) {
            Entry::Occupied(_) => Err(OagwError::new(
                ErrorKind::AliasConflict,
                format!(
                    "a plugin named {:?} already exists for this tenant",
                    plugin.name
                ),
            )),
            Entry::Vacant(slot) => {
                slot.insert(plugin.id);
                self.0.plugins.insert(plugin.id, plugin.clone());
                Ok(plugin)
            }
        }
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<PluginRecord>> {
        Ok(self
            .0
            .plugins
            .get(&id)
            .filter(|entry| entry.value().tenant_id == tenant_id)
            .map(|entry| entry.value().clone()))
    }

    async fn get_unscoped(&self, id: Uuid) -> OagwResult<Option<PluginRecord>> {
        Ok(self.0.plugins.get(&id).map(|entry| entry.value().clone()))
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool> {
        let Some((_, plugin)) = self
            .0
            .plugins
            .remove_if(&id, |_, value| value.tenant_id == tenant_id)
        else {
            return Ok(false);
        };
        self.0
            .plugin_name_index
            .remove(&(plugin.tenant_id, plugin.name));
        Ok(true)
    }

    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<PluginRecord>> {
        let mut found: Vec<PluginRecord> = self
            .0
            .plugins
            .iter()
            .filter(|entry| entry.value().tenant_id == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        found.sort_by_key(|p| p.seq);
        Ok(found)
    }

    async fn set_gc_eligible(&self, id: Uuid, at: Option<u64>) -> OagwResult<()> {
        if let Some(mut entry) = self.0.plugins.get_mut(&id) {
            entry.value_mut().gc_eligible_at = at;
        }
        Ok(())
    }
}

/// [`PluginUsageRepository`] over [`MemoryStore`].
#[derive(Debug, Clone)]
pub struct MemoryPluginUsageRepo(Arc<MemoryStore>);

impl MemoryPluginUsageRepo {
    /// Wrap a shared store.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

#[async_trait]
impl PluginUsageRepository for MemoryPluginUsageRepo {
    async fn references(&self, plugin_ref: &str) -> OagwResult<PluginUsage> {
        let refs = self.0.plugin_references(plugin_ref);
        Ok(PluginUsage {
            upstreams: refs.upstreams,
            routes: refs.routes,
        })
    }

    fn clock(&self) -> u64 {
        self.0.clock()
    }

    async fn collect_garbage(&self, now: u64) -> OagwResult<usize> {
        Ok(self.0.collect_garbage(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::PluginKind;
    use crate::domain::model::{
        Endpoint, HttpMatch, MatchConfig, PathSuffixMode, Protocol, Scheme, ServerConfig,
    };

    fn upstream(tenant: Uuid, alias: &str, seq: u64) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            enabled: true,
            protocol: Protocol::Http,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: alias.to_owned(),
                    port: None,
                }],
            },
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
            seq,
        }
    }

    fn route(tenant: Uuid, upstream_id: Uuid, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            enabled: true,
            priority: 0,
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: vec![],
            seq: 0,
        }
    }

    #[tokio::test]
    async fn alias_is_unique_per_tenant_not_globally() {
        let store = Arc::new(MemoryStore::new());
        let repo = MemoryUpstreamRepo::new(Arc::clone(&store));
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();

        repo.create(upstream(tenant_a, "api.openai.com", 0))
            .await
            .expect("first create");
        let conflict = repo
            .create(upstream(tenant_a, "api.openai.com", 1))
            .await
            .expect_err("same tenant + alias conflicts");
        assert_eq!(conflict.kind(), ErrorKind::AliasConflict);

        repo.create(upstream(tenant_b, "api.openai.com", 2))
            .await
            .expect("a different tenant may shadow the alias");
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let store = Arc::new(MemoryStore::new());
        let repo = MemoryUpstreamRepo::new(Arc::clone(&store));
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let created = repo
            .create(upstream(owner, "a.example.com", 0))
            .await
            .unwrap();

        assert!(repo.get(owner, created.id).await.unwrap().is_some());
        assert!(
            repo.get(other, created.id).await.unwrap().is_none(),
            "another tenant cannot see the row"
        );
        assert!(!repo.delete(other, created.id).await.unwrap());
        assert!(repo.get(owner, created.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn deleting_an_upstream_cascades_to_routes() {
        let store = Arc::new(MemoryStore::new());
        let upstreams = MemoryUpstreamRepo::new(Arc::clone(&store));
        let routes = MemoryRouteRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let created = upstreams
            .create(upstream(tenant, "a.example.com", 0))
            .await
            .unwrap();
        routes
            .create(route(tenant, created.id, "/v1"))
            .await
            .unwrap();
        assert_eq!(routes.list_by_upstream(created.id).await.unwrap().len(), 1);

        assert!(upstreams.delete(tenant, created.id).await.unwrap());
        assert!(
            routes
                .list_by_upstream(created.id)
                .await
                .unwrap()
                .is_empty()
        );
        // The alias is free again after the delete.
        upstreams
            .create(upstream(tenant, "a.example.com", 1))
            .await
            .expect("alias released on delete");
    }

    #[tokio::test]
    async fn plugin_names_are_unique_per_tenant() {
        let store = Arc::new(MemoryStore::new());
        let repo = MemoryPluginRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let plugin = PluginRecord {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            plugin_type: PluginKind::Guard,
            name: "request_validator".to_owned(),
            description: None,
            phases: vec!["on_request".to_owned()],
            config_schema: None,
            source_code: "def on_request(ctx):\n    return ctx.next()\n".to_owned(),
            gc_eligible_at: None,
            seq: 0,
        };
        repo.create(plugin.clone()).await.expect("first create");
        let dup = PluginRecord {
            id: Uuid::new_v4(),
            ..plugin
        };
        assert!(repo.create(dup).await.is_err());
    }

    #[tokio::test]
    async fn plugin_references_find_chain_and_auth_bindings() {
        use crate::domain::model::{AuthConfig, PluginBinding, PluginConfig, PluginsConfig};

        let store = Arc::new(MemoryStore::new());
        let upstreams = MemoryUpstreamRepo::new(Arc::clone(&store));
        let routes = MemoryRouteRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let plugin_ref = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

        let mut with_chain = upstream(tenant, "a.example.com", 0);
        with_chain.plugins = Some(PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![PluginBinding {
                plugin_ref: plugin_ref.to_owned(),
                config: PluginConfig::new(),
            }],
        });
        let created = upstreams.create(with_chain).await.unwrap();

        let mut with_auth = upstream(tenant, "b.example.com", 1);
        with_auth.auth = Some(AuthConfig {
            plugin_type: plugin_ref.to_owned(),
            sharing: crate::domain::model::SharingMode::Private,
            config: PluginConfig::new(),
        });
        upstreams.create(with_auth).await.unwrap();

        let mut route_with_chain = route(tenant, created.id, "/v1");
        route_with_chain.plugins = Some(PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![PluginBinding {
                plugin_ref: plugin_ref.to_owned(),
                config: PluginConfig::new(),
            }],
        });
        routes.create(route_with_chain).await.unwrap();

        let refs = store.plugin_references(plugin_ref);
        assert_eq!(refs.upstreams.len(), 2);
        assert_eq!(refs.routes.len(), 1);
        assert!(!refs.is_empty());
        assert!(
            store
                .plugin_references("gts.cf.core.oagw.guard_plugin.v1~unused.v1")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn garbage_collection_removes_expired_plugins() {
        let store = Arc::new(MemoryStore::new());
        let repo = MemoryPluginRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let id = Uuid::new_v4();
        repo.create(PluginRecord {
            id,
            tenant_id: tenant,
            plugin_type: PluginKind::Transform,
            name: "redact".to_owned(),
            description: None,
            phases: vec![],
            config_schema: None,
            source_code: String::new(),
            gc_eligible_at: None,
            seq: 0,
        })
        .await
        .unwrap();

        assert_eq!(store.collect_garbage(100), 0);
        repo.set_gc_eligible(id, Some(50)).await.unwrap();
        assert_eq!(store.collect_garbage(49), 0, "deadline not reached");
        assert_eq!(store.collect_garbage(50), 1);
        assert!(repo.get(tenant, id).await.unwrap().is_none());
    }
}
