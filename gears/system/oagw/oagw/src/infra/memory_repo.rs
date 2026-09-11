//! In-memory implementations of the domain repository ports.
//!
//! The e2e deployment of this gear carries no database configuration, so the
//! control plane keeps its state in process. The traits stay in place so a
//! database-backed implementation can be dropped in without touching callers.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Key of a tenant-scoped resource.
type TenantKey = (Uuid, Uuid);

/// Shared, lock-protected map.
type Shared<T> = Arc<RwLock<HashMap<TenantKey, T>>>;

/// In-memory upstream store.
#[derive(Debug, Clone, Default)]
pub struct MemoryUpstreamRepository {
    rows: Shared<Upstream>,
}

impl MemoryUpstreamRepository {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn rows(&self) -> parking_lot::RwLockReadGuard<'_, HashMap<TenantKey, Upstream>> {
        self.rows.read()
    }
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamRepository {
    async fn insert(&self, upstream: Upstream) -> Result<(), DomainError> {
        let key = (upstream.tenant_id, upstream.id);
        let alias = upstream.alias.clone();
        let mut rows = self.rows.write();
        if rows
            .values()
            .any(|u| u.tenant_id == upstream.tenant_id && u.alias == alias)
        {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias '{alias}' already exists"
            )));
        }
        rows.insert(key, upstream);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        Ok(self.rows().get(&(tenant_id, id)).cloned())
    }

    async fn find_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        let alias = alias.to_ascii_lowercase();
        Ok(self
            .rows()
            .values()
            .find(|u| u.tenant_id == tenant_id && u.alias.eq_ignore_ascii_case(&alias))
            .cloned())
    }

    async fn list_by_tenant(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        let mut rows: Vec<Upstream> = self
            .rows()
            .values()
            .filter(|u| u.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.alias.cmp(&b.alias));
        Ok(rows)
    }

    async fn list_all(&self) -> Result<Vec<Upstream>, DomainError> {
        Ok(self.rows().values().cloned().collect())
    }

    async fn replace(&self, upstream: Upstream) -> Result<(), DomainError> {
        let key = (upstream.tenant_id, upstream.id);
        let alias = upstream.alias.clone();
        let mut rows = self.rows.write();
        if rows
            .values()
            .any(|u| u.tenant_id == upstream.tenant_id && u.alias == alias && u.id != upstream.id)
        {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias '{alias}' already exists"
            )));
        }
        rows.insert(key, upstream);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.rows.write().remove(&(tenant_id, id)).is_some())
    }
}

/// In-memory route store.
#[derive(Debug, Clone, Default)]
pub struct MemoryRouteRepository {
    rows: Shared<Route>,
}

impl MemoryRouteRepository {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The match key used to detect duplicate routes.
    pub fn match_key(route: &Route) -> Option<(Uuid, String, Vec<String>, i64)> {
        let http = route.match_rule.http.as_ref()?;
        let mut methods: Vec<String> = http.methods.iter().map(String::from).collect();
        methods.sort();
        Some((
            route.upstream_id,
            http.path.clone(),
            methods,
            route.priority,
        ))
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteRepository {
    async fn insert(&self, route: Route) -> Result<(), DomainError> {
        let key = (route.tenant_id, route.id);
        let dup = Self::match_key(&route);
        let mut rows = self.rows.write();
        if let Some(dup) = &dup
            && rows
                .values()
                .any(|r| r.enabled && Self::match_key(r).as_ref() == Some(dup) && r.id != route.id)
        {
            return Err(DomainError::Conflict(
                "a route with the same match rule already exists for this upstream".to_owned(),
            ));
        }
        rows.insert(key, route);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self.rows.read().get(&(tenant_id, id)).cloned())
    }

    async fn list_by_tenant(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        let mut rows: Vec<Route> = self
            .rows
            .read()
            .values()
            .filter(|r| r.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.priority));
        Ok(rows)
    }

    async fn list_all(&self) -> Result<Vec<Route>, DomainError> {
        Ok(self.rows.read().values().cloned().collect())
    }

    async fn replace(&self, route: Route) -> Result<(), DomainError> {
        let key = (route.tenant_id, route.id);
        let dup = Self::match_key(&route);
        let mut rows = self.rows.write();
        if let Some(dup) = &dup
            && rows
                .values()
                .any(|r| r.enabled && Self::match_key(r).as_ref() == Some(dup) && r.id != route.id)
        {
            return Err(DomainError::Conflict(
                "a route with the same match rule already exists for this upstream".to_owned(),
            ));
        }
        rows.insert(key, route);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.rows.write().remove(&(tenant_id, id)).is_some())
    }
}

/// In-memory plugin store.
#[derive(Debug, Clone, Default)]
pub struct MemoryPluginRepository {
    rows: Shared<Plugin>,
}

impl MemoryPluginRepository {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginRepository {
    async fn insert(&self, plugin: Plugin) -> Result<(), DomainError> {
        let key = (plugin.tenant_id, plugin.id);
        let name = plugin.name.clone();
        let mut rows = self.rows.write();
        if rows
            .values()
            .any(|p| p.tenant_id == plugin.tenant_id && p.name == name)
        {
            return Err(DomainError::Conflict(format!(
                "a plugin named '{name}' already exists"
            )));
        }
        rows.insert(key, plugin);
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        Ok(self.rows.read().get(&(tenant_id, id)).cloned())
    }

    async fn list_by_tenant(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        let mut rows: Vec<Plugin> = self
            .rows
            .read()
            .values()
            .filter(|p| p.tenant_id == tenant_id)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(rows)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.rows.write().remove(&(tenant_id, id)).is_some())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    async fn store() -> MemoryUpstreamRepository {
        let repo = MemoryUpstreamRepository::new();
        repo.insert(Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            alias: "a.example.com".to_owned(),
            protocol: "p".to_owned(),
            enabled: true,
            server: crate::domain::model::ServerConfig { endpoints: vec![] },
            auth: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: vec![],
        })
        .await
        .unwrap();
        repo
    }

    #[tokio::test]
    async fn duplicate_alias_conflicts() {
        let repo = store().await;
        let err = repo
            .insert(Upstream {
                id: Uuid::new_v4(),
                tenant_id: Uuid::nil(),
                alias: "a.example.com".to_owned(),
                protocol: "p".to_owned(),
                enabled: true,
                server: crate::domain::model::ServerConfig { endpoints: vec![] },
                auth: None,
                headers: None,
                rate_limit: None,
                cors: None,
                plugins: None,
                tags: vec![],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Conflict(_)));
    }
}
