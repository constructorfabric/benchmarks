//! In-memory tenant-scoped repositories.
//!
//! Serves the single-node control plane: [`DashMap`]s keyed by resource
//! id, with tenant scoping enforced at read/write time. Ancestor
//! resources are invisible to descendants on the management plane (§1.2);
//! the alias-resolution walk (with shadowing) is the only hierarchy-aware
//! read, and it is expressed via the explicit `tenant_chain` parameters.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepo, RouteRepo, UpstreamRepo};

/// The full in-memory store behind every repo. One instance is shared
/// across all tenants (single-node control plane).
#[derive(Default)]
pub struct MemoryStore {
    upstreams: DashMap<Uuid, Upstream>,
    routes: DashMap<Uuid, Route>,
    plugins: DashMap<Uuid, Plugin>,
}

impl MemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The upstream repository view.
    #[must_use]
    pub fn upstreams(self: Arc<Self>) -> MemoryUpstreamRepo {
        MemoryUpstreamRepo { store: self }
    }

    /// The route repository view.
    #[must_use]
    pub fn routes(self: Arc<Self>) -> MemoryRouteRepo {
        MemoryRouteRepo { store: self }
    }

    /// The plugin repository view.
    #[must_use]
    pub fn plugins(self: Arc<Self>) -> MemoryPluginRepo {
        MemoryPluginRepo { store: self }
    }
}

/// Owner check helper shared by every repo.
fn not_found(resource: &str, id: Uuid) -> DomainError {
    DomainError::not_found(resource, format!("resource '{id}' is not visible to this tenant"))
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// In-memory [`UpstreamRepo`] over a shared [`MemoryStore`].
pub struct MemoryUpstreamRepo {
    store: Arc<MemoryStore>,
}

#[async_trait]
impl UpstreamRepo for MemoryUpstreamRepo {
    async fn insert(&self, upstream: Upstream) -> Result<(), DomainError> {
        if self.store.upstreams.contains_key(&upstream.id) {
            return Err(DomainError::already_exists(
                "upstream",
                format!("upstream '{}' already exists", upstream.id),
            ));
        }
        self.store.upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        let found = self
            .store
            .upstreams
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.clone())
            .ok_or_else(|| not_found("upstream", id))?;
        Ok(found)
    }

    async fn replace(&self, tenant_id: Uuid, upstream: Upstream) -> Result<(), DomainError> {
        let owned = self
            .store
            .upstreams
            .get(&upstream.id)
            .is_some_and(|u| u.tenant_id == tenant_id);
        if !owned {
            return Err(not_found("upstream", upstream.id));
        }
        self.store.upstreams.insert(upstream.id, upstream);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let owned = self
            .store
            .upstreams
            .get(&id)
            .is_some_and(|u| u.tenant_id == tenant_id);
        if !owned {
            return Err(not_found("upstream", id));
        }
        self.store.upstreams.remove(&id);
        Ok(())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut out: Vec<Upstream> = self
            .store
            .upstreams
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.clone())
            .collect();
        out.sort_by_key(|u| u.alias.clone());
        out
    }

    async fn alias_exists(&self, tenant_id: Uuid, alias: &str) -> bool {
        self.store
            .upstreams
            .iter()
            .any(|u| u.tenant_id == tenant_id && u.alias == alias)
    }

    async fn resolve_alias(&self, tenant_chain: &[Uuid], alias: &str) -> Option<Upstream> {
        for tenant in tenant_chain {
            // Closest tenant in the chain shadows outer tenants.
            if let Some(found) = self
                .store
                .upstreams
                .iter()
                .find(|u| u.tenant_id == *tenant && u.alias == alias)
            {
                return Some(found.clone());
            }
        }
        None
    }

    async fn get_any_tenant(&self, id: Uuid) -> Option<Upstream> {
        self.store.upstreams.get(&id).map(|u| u.clone())
    }

    async fn all_upstreams(&self) -> Vec<Upstream> {
        self.store
            .upstreams
            .iter()
            .map(|u| u.clone())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// In-memory [`RouteRepo`] over a shared [`MemoryStore`].
pub struct MemoryRouteRepo {
    store: Arc<MemoryStore>,
}

#[async_trait]
impl RouteRepo for MemoryRouteRepo {
    async fn insert(&self, route: Route) -> Result<(), DomainError> {
        if self.store.routes.contains_key(&route.id) {
            return Err(DomainError::already_exists(
                "route",
                format!("route '{}' already exists", route.id),
            ));
        }
        self.store.routes.insert(route.id, route);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.store
            .routes
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .ok_or_else(|| not_found("route", id))
    }

    async fn replace(&self, tenant_id: Uuid, route: Route) -> Result<(), DomainError> {
        let owned = self
            .store
            .routes
            .get(&route.id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if !owned {
            return Err(not_found("route", route.id));
        }
        self.store.routes.insert(route.id, route);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let owned = self
            .store
            .routes
            .get(&id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if !owned {
            return Err(not_found("route", id));
        }
        self.store.routes.remove(&id);
        Ok(())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut out: Vec<Route> = self
            .store
            .routes
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.id);
        out
    }

    async fn list_for_chain(&self, tenant_chain: &[Uuid]) -> Vec<Route> {
        let mut out: Vec<Route> = self
            .store
            .routes
            .iter()
            .filter(|r| tenant_chain.contains(&r.tenant_id))
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.id);
        out
    }

    async fn get_any_tenant(&self, id: Uuid) -> Option<Route> {
        self.store.routes.get(&id).map(|r| r.clone())
    }

    async fn all_routes(&self) -> Vec<Route> {
        self.store.routes.iter().map(|r| r.clone()).collect()
    }
}

// ---------------------------------------------------------------------------
// Custom plugins
// ---------------------------------------------------------------------------

/// In-memory [`PluginRepo`] over a shared [`MemoryStore`].
pub struct MemoryPluginRepo {
    store: Arc<MemoryStore>,
}

#[async_trait]
impl PluginRepo for MemoryPluginRepo {
    async fn insert(&self, plugin: Plugin) -> Result<(), DomainError> {
        if self.store.plugins.contains_key(&plugin.id) {
            return Err(DomainError::already_exists(
                "plugin",
                format!("plugin '{}' already exists", plugin.id),
            ));
        }
        self.store.plugins.insert(plugin.id, plugin);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.store
            .plugins
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .map(|p| p.clone())
            .ok_or_else(|| not_found("plugin", id))
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let owned = self
            .store
            .plugins
            .get(&id)
            .is_some_and(|p| p.tenant_id == tenant_id);
        if !owned {
            return Err(not_found("plugin", id));
        }
        self.store.plugins.remove(&id);
        Ok(())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Plugin> {
        let mut out: Vec<Plugin> = self
            .store
            .plugins
            .iter()
            .filter(|p| p.tenant_id == tenant_id)
            .map(|p| p.clone())
            .collect();
        out.sort_by_key(|p| p.name.clone());
        out
    }

    async fn get_any_tenant(&self, id: Uuid) -> Option<Plugin> {
        self.store.plugins.get(&id).map(|p| p.clone())
    }

    async fn all_plugins(&self) -> Vec<Plugin> {
        self.store.plugins.iter().map(|p| p.clone()).collect()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{
        Endpoint, EndpointScheme, HttpMatch, HttpMethod, PathSuffixMode, PluginKind, RouteMatch,
        ServerConfig, UpstreamProtocol,
    };

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: true,
            alias: alias.to_owned(),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.example.com".to_owned(),
                    port: None,
                }],
            },
            protocol: UpstreamProtocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn route(tenant: Uuid, upstream_id: Uuid) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            tags: vec![],
            upstream_id,
            match_: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: "/v1".to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[tokio::test]
    async fn tenant_scoping_hides_other_tenants() {
        let store = Arc::new(MemoryStore::new());
        let repo = store.upstreams();
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let ua = upstream(tenant_a, "a.example.com");
        repo.insert(ua.clone()).await.unwrap();

        // Own tenant sees it.
        assert!(repo.get(tenant_a, ua.id).await.is_ok());
        // Other tenant does not.
        assert!(repo.get(tenant_b, ua.id).await.is_err());
        assert_eq!(repo.list(tenant_b).await.len(), 0);
    }

    #[tokio::test]
    async fn alias_shadowing_prefers_closest_tenant() {
        let store = Arc::new(MemoryStore::new());
        let repo = store.upstreams();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let root_up = upstream(root, "api.example.com");
        let child_up = Upstream {
            alias: "api.example.com".to_owned(),
            ..upstream(child, "api.example.com")
        };
        repo.insert(root_up.clone()).await.unwrap();
        repo.insert(child_up.clone()).await.unwrap();

        // Child's chain (child → root): closest wins.
        let found = repo
            .resolve_alias(&[child, root], "api.example.com")
            .await
            .expect("resolved");
        assert_eq!(found.tenant_id, child);
        // Root alone resolves its own.
        let found = repo.resolve_alias(&[root], "api.example.com").await.unwrap();
        assert_eq!(found.tenant_id, root);
    }

    #[tokio::test]
    async fn route_repo_chain_listing() {
        let store = Arc::new(MemoryStore::new());
        let repo = store.routes();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        repo.insert(route(root, upstream_id)).await.unwrap();
        repo.insert(route(child, upstream_id)).await.unwrap();

        assert_eq!(repo.list(root).await.len(), 1);
        assert_eq!(repo.list_for_chain(&[child, root]).await.len(), 2);
    }

    #[tokio::test]
    async fn plugin_repo_round_trip() {
        let store = Arc::new(MemoryStore::new());
        let repo = store.plugins();
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            kind: PluginKind::Guard,
            builtin_type: crate::gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
            name: "my-guard".to_owned(),
            config: serde_json::json!({"required_request_headers": "X-Api-Version"}),
        };
        repo.insert(plugin.clone()).await.unwrap();
        let got = repo.get(plugin.tenant_id, plugin.id).await.unwrap();
        assert_eq!(got.name, "my-guard");
        repo.delete(plugin.tenant_id, plugin.id).await.unwrap();
        assert!(repo.get(plugin.tenant_id, plugin.id).await.is_err());
    }
}
