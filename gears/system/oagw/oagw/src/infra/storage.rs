//! In-process repositories.
//!
//! The gear declares no `db` capability, so configuration lives in memory for
//! the lifetime of the process. Every read and write is tenant-scoped at this
//! boundary, which is where a SeaORM implementation would enforce the same
//! invariant through the secure ORM.

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// In-memory upstream store keyed by id, with an alias index per tenant.
#[derive(Debug, Default)]
pub struct InMemoryUpstreamRepo {
    by_id: DashMap<Uuid, Upstream>,
}

impl InMemoryUpstreamRepo {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl UpstreamRepository for InMemoryUpstreamRepo {
    async fn create(&self, upstream: Upstream) -> DomainResult<Upstream> {
        let clash = self.by_id.iter().any(|entry| {
            entry.tenant_id == upstream.tenant_id && entry.alias == upstream.alias
        });
        if clash {
            return Err(DomainError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                upstream.alias
            )));
        }
        self.by_id.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn replace(&self, upstream: Upstream) -> DomainResult<Upstream> {
        let exists = self
            .by_id
            .get(&upstream.id)
            .is_some_and(|existing| existing.tenant_id == upstream.tenant_id);
        if !exists {
            return Err(DomainError::not_found(format!(
                "upstream '{}' not found",
                upstream.id
            )));
        }
        let clash = self.by_id.iter().any(|entry| {
            entry.id != upstream.id
                && entry.tenant_id == upstream.tenant_id
                && entry.alias == upstream.alias
        });
        if clash {
            return Err(DomainError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                upstream.alias
            )));
        }
        self.by_id.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Upstream>> {
        Ok(self
            .by_id
            .get(&id)
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.clone()))
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> DomainResult<Option<Upstream>> {
        Ok(self
            .by_id
            .iter()
            .find(|u| u.tenant_id == tenant_id && u.alias == alias)
            .map(|u| u.clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> DomainResult<Vec<Upstream>> {
        let mut out: Vec<Upstream> = self
            .by_id
            .iter()
            .filter(|u| u.tenant_id == tenant_id)
            .map(|u| u.clone())
            .collect();
        out.sort_by(|a, b| a.alias.cmp(&b.alias));
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let owned = self
            .by_id
            .get(&id)
            .is_some_and(|u| u.tenant_id == tenant_id);
        if !owned {
            return Ok(false);
        }
        Ok(self.by_id.remove(&id).is_some())
    }

    async fn all(&self) -> DomainResult<Vec<Upstream>> {
        Ok(self.by_id.iter().map(|u| u.clone()).collect())
    }
}

/// In-memory route store keyed by id.
#[derive(Debug, Default)]
pub struct InMemoryRouteRepo {
    by_id: DashMap<Uuid, Route>,
}

impl InMemoryRouteRepo {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RouteRepository for InMemoryRouteRepo {
    async fn create(&self, route: Route) -> DomainResult<Route> {
        self.by_id.insert(route.id, route.clone());
        Ok(route)
    }

    async fn replace(&self, route: Route) -> DomainResult<Route> {
        let exists = self
            .by_id
            .get(&route.id)
            .is_some_and(|existing| existing.tenant_id == route.tenant_id);
        if !exists {
            return Err(DomainError::not_found(format!(
                "route '{}' not found",
                route.id
            )));
        }
        self.by_id.insert(route.id, route.clone());
        Ok(route)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Route>> {
        Ok(self
            .by_id
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> DomainResult<Vec<Route>> {
        let mut out: Vec<Route> = self
            .by_id
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.id);
        Ok(out)
    }

    async fn list_by_upstream(&self, upstream_id: Uuid) -> DomainResult<Vec<Route>> {
        let mut out: Vec<Route> = self
            .by_id
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.id);
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let owned = self
            .by_id
            .get(&id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if !owned {
            return Ok(false);
        }
        Ok(self.by_id.remove(&id).is_some())
    }

    async fn delete_by_upstream(&self, upstream_id: Uuid) -> DomainResult<usize> {
        let doomed: Vec<Uuid> = self
            .by_id
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .map(|r| r.id)
            .collect();
        let count = doomed.len();
        for id in doomed {
            self.by_id.remove(&id);
        }
        Ok(count)
    }

    async fn all(&self) -> DomainResult<Vec<Route>> {
        Ok(self.by_id.iter().map(|r| r.clone()).collect())
    }
}

/// In-memory plugin-definition store keyed by id.
#[derive(Debug, Default)]
pub struct InMemoryPluginRepo {
    by_id: DashMap<Uuid, Plugin>,
}

impl InMemoryPluginRepo {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PluginRepository for InMemoryPluginRepo {
    async fn create(&self, plugin: Plugin) -> DomainResult<Plugin> {
        let clash = self
            .by_id
            .iter()
            .any(|p| p.tenant_id == plugin.tenant_id && p.name == plugin.name);
        if clash {
            return Err(DomainError::conflict(format!(
                "a plugin named '{}' already exists for this tenant",
                plugin.name
            )));
        }
        self.by_id.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Plugin>> {
        Ok(self
            .by_id
            .get(&id)
            .filter(|p| p.tenant_id == tenant_id)
            .map(|p| p.clone()))
    }

    async fn get_any(&self, id: Uuid) -> DomainResult<Option<Plugin>> {
        Ok(self.by_id.get(&id).map(|p| p.clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> DomainResult<Vec<Plugin>> {
        let mut out: Vec<Plugin> = self
            .by_id
            .iter()
            .filter(|p| p.tenant_id == tenant_id)
            .map(|p| p.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let owned = self
            .by_id
            .get(&id)
            .is_some_and(|p| p.tenant_id == tenant_id);
        if !owned {
            return Ok(false);
        }
        Ok(self.by_id.remove(&id).is_some())
    }

    async fn set_gc_eligible_at(&self, id: Uuid, at_epoch_secs: Option<u64>) -> DomainResult<()> {
        if let Some(mut entry) = self.by_id.get_mut(&id) {
            entry.gc_eligible_at = at_epoch_secs;
        }
        Ok(())
    }

    async fn collect_garbage(&self, now_epoch_secs: u64) -> DomainResult<usize> {
        let doomed: Vec<Uuid> = self
            .by_id
            .iter()
            .filter(|p| p.gc_eligible_at.is_some_and(|at| at <= now_epoch_secs))
            .map(|p| p.id)
            .collect();
        let count = doomed.len();
        for id in doomed {
            self.by_id.remove(&id);
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers;
    use crate::domain::model::{
        Endpoint, HeadersConfig, PluginKind, PluginsConfig, Scheme, ServerConfig,
    };

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: alias.to_owned(),
                    port: Some(443),
                }],
            },
            protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn plugin(tenant: Uuid, name: &str) -> Plugin {
        Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            name: name.to_owned(),
            description: None,
            plugin_type: PluginKind::Guard,
            phases: Vec::new(),
            config_schema: serde_json::json!({}),
            source_code: "def on_request(ctx): return ctx.next()".to_owned(),
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    #[tokio::test]
    async fn alias_is_unique_per_tenant_not_globally() {
        let repo = InMemoryUpstreamRepo::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        repo.create(upstream(a, "api.openai.com"))
            .await
            .expect("first create");
        repo.create(upstream(b, "api.openai.com"))
            .await
            .expect("other tenant may shadow");
        let err = repo
            .create(upstream(a, "api.openai.com"))
            .await
            .expect_err("same tenant conflicts");
        assert_eq!(err.status(), 409);
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let repo = InMemoryUpstreamRepo::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let created = repo
            .create(upstream(owner, "api.openai.com"))
            .await
            .expect("create");
        assert!(repo.get(owner, created.id).await.expect("get").is_some());
        assert!(repo.get(other, created.id).await.expect("get").is_none());
        assert!(!repo.delete(other, created.id).await.expect("delete"));
        assert!(repo.delete(owner, created.id).await.expect("delete"));
    }

    #[tokio::test]
    async fn plugin_names_are_unique_per_tenant() {
        let repo = InMemoryPluginRepo::new();
        let tenant = Uuid::new_v4();
        repo.create(plugin(tenant, "validator"))
            .await
            .expect("create");
        let err = repo
            .create(plugin(tenant, "validator"))
            .await
            .expect_err("duplicate name");
        assert_eq!(err.status(), 409);
    }

    #[tokio::test]
    async fn garbage_collection_removes_only_expired_entries() {
        let repo = InMemoryPluginRepo::new();
        let tenant = Uuid::new_v4();
        let expired = repo.create(plugin(tenant, "expired")).await.expect("create");
        let fresh = repo.create(plugin(tenant, "fresh")).await.expect("create");
        repo.set_gc_eligible_at(expired.id, Some(100))
            .await
            .expect("mark");
        repo.set_gc_eligible_at(fresh.id, Some(1_000))
            .await
            .expect("mark");
        assert_eq!(repo.collect_garbage(500).await.expect("gc"), 1);
        assert!(repo.get(tenant, fresh.id).await.expect("get").is_some());
        assert!(repo.get(tenant, expired.id).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn deleting_an_upstream_cascades_to_its_routes() {
        let routes = InMemoryRouteRepo::new();
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        for _ in 0..3 {
            routes
                .create(Route {
                    id: Uuid::new_v4(),
                    tenant_id: tenant,
                    upstream_id,
                    enabled: true,
                    priority: 0,
                    tags: Vec::new(),
                    match_config: crate::domain::model::MatchConfig::default(),
                    plugins: PluginsConfig::default(),
                    rate_limit: None,
                    cors: None,
                })
                .await
                .expect("create");
        }
        assert_eq!(
            routes
                .delete_by_upstream(upstream_id)
                .await
                .expect("cascade"),
            3
        );
        assert!(routes.all().await.expect("all").is_empty());
    }
}
