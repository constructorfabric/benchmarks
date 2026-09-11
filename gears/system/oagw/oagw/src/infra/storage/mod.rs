//! Repository implementations.
//!
//! OAGW is configured without a `database:` block in the deployed
//! configuration, so `GearCtx::db()` yields nothing and the gear owns its
//! configuration in process. The repositories below keep the same tenant
//! scoping invariants the SQL schema encodes: every read and write is keyed on
//! `(tenant_id, …)` and `oagw_route.upstream_id` cascades on delete.

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::OagwResult;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// In-memory `oagw_upstream` with the `UNIQUE (tenant_id, alias)` index.
#[derive(Default)]
pub struct InMemoryUpstreamRepo {
    rows: DashMap<Uuid, Upstream>,
}

impl InMemoryUpstreamRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl UpstreamRepository for InMemoryUpstreamRepo {
    async fn insert(&self, upstream: Upstream) -> OagwResult<Upstream> {
        self.rows.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Upstream>> {
        Ok(self
            .rows
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone()))
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> OagwResult<Option<Upstream>> {
        Ok(self
            .rows
            .iter()
            .find(|r| r.tenant_id == tenant_id && r.alias == alias)
            .map(|r| r.clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>> {
        let mut out: Vec<Upstream> = self
            .rows
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn replace(&self, upstream: Upstream) -> OagwResult<Upstream> {
        self.rows.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool> {
        let owned = self
            .rows
            .get(&id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if owned {
            self.rows.remove(&id);
        }
        Ok(owned)
    }

    async fn all(&self) -> OagwResult<Vec<Upstream>> {
        Ok(self.rows.iter().map(|r| r.clone()).collect())
    }
}

/// In-memory `oagw_route` (plus its match-key and tag side tables).
#[derive(Default)]
pub struct InMemoryRouteRepo {
    rows: DashMap<Uuid, Route>,
}

impl InMemoryRouteRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RouteRepository for InMemoryRouteRepo {
    async fn insert(&self, route: Route) -> OagwResult<Route> {
        self.rows.insert(route.id, route.clone());
        Ok(route)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Route>> {
        Ok(self
            .rows
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>> {
        let mut out: Vec<Route> = self
            .rows
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn list_by_upstream(&self, upstream_id: Uuid) -> OagwResult<Vec<Route>> {
        let mut out: Vec<Route> = self
            .rows
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn replace(&self, route: Route) -> OagwResult<Route> {
        self.rows.insert(route.id, route.clone());
        Ok(route)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool> {
        let owned = self
            .rows
            .get(&id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if owned {
            self.rows.remove(&id);
        }
        Ok(owned)
    }

    async fn delete_by_upstream(&self, upstream_id: Uuid) -> OagwResult<usize> {
        let doomed: Vec<Uuid> = self
            .rows
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .map(|r| r.id)
            .collect();
        let n = doomed.len();
        for id in doomed {
            self.rows.remove(&id);
        }
        Ok(n)
    }

    async fn all(&self) -> OagwResult<Vec<Route>> {
        Ok(self.rows.iter().map(|r| r.clone()).collect())
    }
}

/// In-memory `oagw_plugin` with the `UNIQUE (tenant_id, name)` index.
#[derive(Default)]
pub struct InMemoryPluginRepo {
    rows: DashMap<Uuid, Plugin>,
}

impl InMemoryPluginRepo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PluginRepository for InMemoryPluginRepo {
    async fn insert(&self, plugin: Plugin) -> OagwResult<Plugin> {
        self.rows.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Option<Plugin>> {
        Ok(self
            .rows
            .get(&id)
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone()))
    }

    async fn get_any_tenant(&self, id: Uuid) -> OagwResult<Option<Plugin>> {
        Ok(self.rows.get(&id).map(|r| r.clone()))
    }

    async fn find_by_name(&self, tenant_id: Uuid, name: &str) -> OagwResult<Option<Plugin>> {
        Ok(self
            .rows
            .iter()
            .find(|r| r.tenant_id == tenant_id && r.name == name)
            .map(|r| r.clone()))
    }

    async fn list(&self, tenant_id: Uuid) -> OagwResult<Vec<Plugin>> {
        let mut out: Vec<Plugin> = self
            .rows
            .iter()
            .filter(|r| r.tenant_id == tenant_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<bool> {
        let owned = self
            .rows
            .get(&id)
            .is_some_and(|r| r.tenant_id == tenant_id);
        if owned {
            self.rows.remove(&id);
        }
        Ok(owned)
    }

    async fn set_gc_eligible_at(
        &self,
        id: Uuid,
        gc_eligible_at: Option<String>,
    ) -> OagwResult<()> {
        if let Some(mut row) = self.rows.get_mut(&id) {
            row.gc_eligible_at = gc_eligible_at;
        }
        Ok(())
    }

    async fn all(&self) -> OagwResult<Vec<Plugin>> {
        Ok(self.rows.iter().map(|r| r.clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{HeadersConfig, PluginsConfig, ServerConfig};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            enabled: true,
            server: ServerConfig { endpoints: vec![] },
            auth: None,
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: vec![],
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let repo = InMemoryUpstreamRepo::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let created = repo.insert(upstream(a, "api.openai.com")).await.expect("insert");

        assert!(repo.get(a, created.id).await.expect("get").is_some());
        assert!(repo.get(b, created.id).await.expect("get").is_none());
        assert!(
            repo.find_by_alias(b, "api.openai.com")
                .await
                .expect("alias")
                .is_none()
        );
        assert_eq!(repo.list(b).await.expect("list").len(), 0);
        assert_eq!(repo.all().await.expect("all").len(), 1);
    }

    #[tokio::test]
    async fn deleting_another_tenants_row_is_a_no_op() {
        let repo = InMemoryUpstreamRepo::new();
        let a = Uuid::new_v4();
        let created = repo.insert(upstream(a, "x.example.com")).await.expect("insert");
        assert!(!repo.delete(Uuid::new_v4(), created.id).await.expect("delete"));
        assert!(repo.delete(a, created.id).await.expect("delete"));
        assert!(!repo.delete(a, created.id).await.expect("delete"));
    }
}
