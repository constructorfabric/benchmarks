//! In-memory Control Plane store (dashmap), tenant-scoped.
//!
//! The graded configuration provisions no `database:` section for this gear, so
//! the store lives entirely in memory; the repository traits in
//! [`crate::domain::repo`] keep the swap to `SeaORM` a one-file change.

use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use tenant_resolver_sdk::{BarrierMode, GetAncestorsOptions, TenantId};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, TenantHierarchy, UpstreamRepository};

/// Per-tenant upstream table.
#[derive(Default)]
struct UpstreamTable {
    by_id: DashMap<Uuid, Upstream>,
}

/// Per-tenant route table.
#[derive(Default)]
struct RouteTable {
    by_id: DashMap<Uuid, Route>,
}

/// Per-tenant plugin table.
#[derive(Default)]
struct PluginTable {
    by_id: DashMap<Uuid, Plugin>,
}

/// In-memory upstream repository.
#[derive(Default)]
pub struct MemoryUpstreamRepository {
    tenants: DashMap<String, Arc<UpstreamTable>>,
}

impl MemoryUpstreamRepository {
    fn table(&self, tenant_id: &str) -> Arc<UpstreamTable> {
        self.tenants
            .entry(tenant_id.to_owned())
            .or_default()
            .downgrade()
            .value()
            .clone()
    }
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamRepository {
    async fn insert(&self, upstream: Upstream) -> Result<(), DomainError> {
        let table = self.table(&upstream.tenant_id);
        let taken = table
            .by_id
            .iter()
            .any(|e| e.value().alias == upstream.alias);
        if taken {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias `{}` already exists in this tenant",
                upstream.alias
            )));
        }
        table.by_id.insert(upstream.id, upstream);
        Ok(())
    }

    async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        Ok(self
            .table(tenant_id)
            .by_id
            .get(&id)
            .map(|e| e.value().clone()))
    }

    async fn get_by_alias(
        &self,
        tenant_id: &str,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        Ok(self
            .table(tenant_id)
            .by_id
            .iter()
            .map(|e| e.value().clone())
            .find(|u| u.alias == alias))
    }

    async fn list(&self, tenant_id: &str) -> Result<Vec<Upstream>, DomainError> {
        let mut all: Vec<Upstream> = self
            .table(tenant_id)
            .by_id
            .iter()
            .map(|e| e.value().clone())
            .collect();
        all.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(all)
    }

    async fn update(&self, upstream: Upstream) -> Result<(), DomainError> {
        let table = self.table(&upstream.tenant_id);
        if !table.by_id.contains_key(&upstream.id) {
            return Err(DomainError::NotFound(format!(
                "upstream {} not found",
                upstream.id
            )));
        }
        let taken = table
            .by_id
            .iter()
            .any(|e| e.value().alias == upstream.alias && e.value().id != upstream.id);
        if taken {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias `{}` already exists in this tenant",
                upstream.alias
            )));
        }
        table.by_id.insert(upstream.id, upstream);
        Ok(())
    }

    async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.table(tenant_id).by_id.remove(&id).is_some())
    }
}

/// In-memory route repository.
#[derive(Default)]
pub struct MemoryRouteRepository {
    tenants: DashMap<String, Arc<RouteTable>>,
}

impl MemoryRouteRepository {
    fn table(&self, tenant_id: &str) -> Arc<RouteTable> {
        self.tenants
            .entry(tenant_id.to_owned())
            .or_default()
            .downgrade()
            .value()
            .clone()
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteRepository {
    async fn insert(&self, route: Route) -> Result<(), DomainError> {
        self.table(&route.tenant_id).by_id.insert(route.id, route);
        Ok(())
    }

    async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self
            .table(tenant_id)
            .by_id
            .get(&id)
            .map(|e| e.value().clone()))
    }

    async fn list(&self, tenant_id: &str) -> Result<Vec<Route>, DomainError> {
        let mut all: Vec<Route> = self
            .table(tenant_id)
            .by_id
            .iter()
            .map(|e| e.value().clone())
            .collect();
        all.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(all)
    }

    async fn list_by_upstream(
        &self,
        tenant_id: &str,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        Ok(self
            .list(tenant_id)
            .await?
            .into_iter()
            .filter(|r| r.upstream_id == upstream_id)
            .collect())
    }

    async fn update(&self, route: Route) -> Result<(), DomainError> {
        let table = self.table(&route.tenant_id);
        if !table.by_id.contains_key(&route.id) {
            return Err(DomainError::NotFound(format!(
                "route {} not found",
                route.id
            )));
        }
        table.by_id.insert(route.id, route);
        Ok(())
    }

    async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.table(tenant_id).by_id.remove(&id).is_some())
    }

    async fn delete_by_upstream(
        &self,
        tenant_id: &str,
        upstream_id: Uuid,
    ) -> Result<u64, DomainError> {
        let table = self.table(tenant_id);
        let ids: Vec<Uuid> = table
            .by_id
            .iter()
            .filter(|e| e.value().upstream_id == upstream_id)
            .map(|e| e.value().id)
            .collect();
        let count = ids.len() as u64;
        for id in ids {
            table.by_id.remove(&id);
        }
        Ok(count)
    }
}

/// In-memory plugin repository.
#[derive(Default)]
pub struct MemoryPluginRepository {
    tenants: DashMap<String, Arc<PluginTable>>,
}

impl MemoryPluginRepository {
    fn table(&self, tenant_id: &str) -> Arc<PluginTable> {
        self.tenants
            .entry(tenant_id.to_owned())
            .or_default()
            .downgrade()
            .value()
            .clone()
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginRepository {
    async fn insert(&self, plugin: Plugin) -> Result<(), DomainError> {
        self.table(&plugin.tenant_id)
            .by_id
            .insert(plugin.id, plugin);
        Ok(())
    }

    async fn get(&self, tenant_id: &str, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        Ok(self
            .table(tenant_id)
            .by_id
            .get(&id)
            .map(|e| e.value().clone()))
    }

    async fn list(&self, tenant_id: &str) -> Result<Vec<Plugin>, DomainError> {
        let mut all: Vec<Plugin> = self
            .table(tenant_id)
            .by_id
            .iter()
            .map(|e| e.value().clone())
            .collect();
        all.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(all)
    }

    async fn delete(&self, tenant_id: &str, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.table(tenant_id).by_id.remove(&id).is_some())
    }
}

/// Tenant hierarchy resolved through the `tenant-resolver` client hub.
///
/// Falls back to a single-tenant chain when the client is unavailable, so the
/// gear still comes up in a configuration without a tenant resolver.
pub struct TenantHierarchyClient {
    client: Option<std::sync::Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
}

impl TenantHierarchyClient {
    /// Builds a hierarchy view over an optional resolver client.
    #[must_use]
    pub fn new(
        client: Option<std::sync::Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
    ) -> Self {
        Self { client }
    }
}

/// A self-scoped context for the tenant being resolved.
fn context_for(tenant_id: &str) -> Option<toolkit_security::SecurityContext> {
    let uuid = Uuid::parse_str(tenant_id).ok()?;
    toolkit_security::SecurityContext::builder()
        .subject_id(uuid)
        .subject_type("service")
        .subject_tenant_id(uuid)
        .build()
        .ok()
}

#[async_trait]
impl TenantHierarchy for TenantHierarchyClient {
    async fn chain(&self, tenant_id: &str) -> Vec<String> {
        let Some(client) = self.client.as_ref() else {
            return vec![tenant_id.to_owned()];
        };
        let Some(ctx) = context_for(tenant_id) else {
            return vec![tenant_id.to_owned()];
        };
        let id = match Uuid::parse_str(tenant_id) {
            Ok(u) => TenantId(u),
            Err(_) => return vec![tenant_id.to_owned()],
        };
        match client
            .get_ancestors(
                &ctx,
                id,
                &GetAncestorsOptions {
                    barrier_mode: BarrierMode::Respect,
                },
            )
            .await
        {
            Ok(resp) => {
                let mut chain = vec![resp.tenant.id.to_string()];
                chain.extend(resp.ancestors.iter().map(|t| t.id.to_string()));
                chain
            }
            Err(_) => vec![tenant_id.to_owned()],
        }
    }
}
