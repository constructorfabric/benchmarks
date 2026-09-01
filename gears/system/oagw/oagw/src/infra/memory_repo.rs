//! In-memory repository (single-instance MVP data plane / control plane
//! store). The dependency set intentionally has no database crate, so
//! persistence is in-process state only.

use async_trait::async_trait;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{PluginRecord, RouteRecord, UpstreamRecord};
use crate::domain::repo::OagwRepository;
use crate::gts;

/// Key for a tenant-scoped resource row.
type TenantKey = (Uuid, Uuid);

/// In-memory implementation of [`OagwRepository`].
///
/// All maps are keyed `(tenant_id, id)` so sibling tenants are never
/// reachable through a lookup.
#[derive(Default)]
pub struct MemoryRepository {
    upstreams: DashMap<TenantKey, UpstreamRecord>,
    // (tenant_id, normalized alias) -> upstream id
    upstream_aliases: DashMap<(Uuid, String), Uuid>,
    routes: DashMap<TenantKey, RouteRecord>,
    plugins: DashMap<TenantKey, PluginRecord>,
}

impl MemoryRepository {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Tenant-scoped map key from an entity id (GTS id or bare UUID).
fn tenant_key(tenant_id: Uuid, id: Option<&str>) -> TenantKey {
    let id = id.and_then(gts::parse_resource_id).unwrap_or_default();
    (tenant_id, id)
}

fn upstream_key(record: &UpstreamRecord) -> TenantKey {
    tenant_key(record.tenant_id, record.entity.id.as_deref())
}

fn route_key(record: &RouteRecord) -> TenantKey {
    tenant_key(record.tenant_id, record.entity.id.as_deref())
}

fn plugin_key(record: &PluginRecord) -> TenantKey {
    tenant_key(record.tenant_id, record.entity.id.as_deref())
}

fn alias_key(tenant_id: Uuid, alias: &str) -> (Uuid, String) {
    (tenant_id, alias.to_ascii_lowercase())
}

#[async_trait]
impl OagwRepository for MemoryRepository {
    async fn insert_upstream(
        &self,
        tenant_id: Uuid,
        upstream: UpstreamRecord,
    ) -> Result<(), DomainError> {
        let alias = upstream
            .entity
            .alias
            .clone()
            .ok_or_else(|| DomainError::internal("insert_upstream called without alias"))?;
        let id = upstream_key(&upstream).1;
        // Claim the alias atomically so concurrent inserts with the same
        // alias cannot both succeed.
        match self.upstream_aliases.entry(alias_key(tenant_id, &alias)) {
            Entry::Occupied(_) => Err(DomainError::Conflict {
                detail: format!("alias {alias:?} is already in use by this tenant"),
            }),
            Entry::Vacant(vacant) => {
                vacant.insert(id);
                self.upstreams.insert((tenant_id, id), upstream);
                Ok(())
            }
        }
    }

    async fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        upstream: UpstreamRecord,
    ) -> Result<(), DomainError> {
        if !self.upstreams.contains_key(&(tenant_id, id)) {
            return Err(DomainError::NotFound);
        }
        let new_alias = upstream
            .entity
            .alias
            .clone()
            .ok_or_else(|| DomainError::internal("update_upstream called without alias"))?;
        let new_key = alias_key(tenant_id, &new_alias);
        if let Some(other) = self.upstream_aliases.get(&new_key)
            && *other != id
        {
            return Err(DomainError::Conflict {
                detail: format!("alias {new_alias:?} is already in use by this tenant"),
            });
        }
        self.upstream_aliases.insert(new_key, id);
        self.upstreams.insert((tenant_id, id), upstream);
        Ok(())
    }

    async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let removed = self.upstreams.remove(&(tenant_id, id));
        let Some((_, record)) = removed else {
            return Err(DomainError::NotFound);
        };
        if let Some(alias) = &record.entity.alias {
            self.upstream_aliases.remove(&alias_key(tenant_id, alias));
        }
        // Cascade: remove routes that referenced this upstream.
        let stale: Vec<TenantKey> = self
            .routes
            .iter()
            .filter(|r| {
                r.key().0 == tenant_id
                    && gts::parse_resource_id(&r.value().entity.upstream_id) == Some(id)
            })
            .map(|r| *r.key())
            .collect();
        for key in stale {
            self.routes.remove(&key);
        }
        Ok(())
    }

    async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRecord, DomainError> {
        self.upstreams
            .get(&(tenant_id, id))
            .map(|r| r.clone())
            .ok_or(DomainError::NotFound)
    }

    async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<UpstreamRecord>, DomainError> {
        let mut out: Vec<UpstreamRecord> = self
            .upstreams
            .iter()
            .filter(|r| r.key().0 == tenant_id)
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| {
            a.entity
                .alias
                .cmp(&b.entity.alias)
                .then(a.entity.id.cmp(&b.entity.id))
        });
        Ok(out)
    }

    async fn list_upstreams_for_tenants(
        &self,
        tenant_ids: &[Uuid],
    ) -> Result<Vec<UpstreamRecord>, DomainError> {
        let mut out: Vec<UpstreamRecord> = self
            .upstreams
            .iter()
            .filter(|r| tenant_ids.contains(&r.key().0))
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| {
            a.entity
                .alias
                .cmp(&b.entity.alias)
                .then(a.entity.id.cmp(&b.entity.id))
        });
        Ok(out)
    }

    async fn insert_route(&self, record: RouteRecord) -> Result<(), DomainError> {
        let key = route_key(&record);
        if self.routes.contains_key(&key) {
            return Err(DomainError::Conflict {
                detail: "route already exists".to_owned(),
            });
        }
        self.routes.insert(key, record);
        Ok(())
    }

    async fn update_route(&self, tenant_id: Uuid, record: RouteRecord) -> Result<(), DomainError> {
        let id = record
            .entity
            .id
            .as_deref()
            .and_then(gts::parse_resource_id)
            .ok_or(DomainError::NotFound)?;
        if !self.routes.contains_key(&(tenant_id, id)) {
            return Err(DomainError::NotFound);
        }
        self.routes.insert(route_key(&record), record);
        Ok(())
    }

    async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.routes
            .remove(&(tenant_id, id))
            .map(|_| ())
            .ok_or(DomainError::NotFound)
    }

    async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRecord, DomainError> {
        self.routes
            .get(&(tenant_id, id))
            .map(|r| r.clone())
            .ok_or(DomainError::NotFound)
    }

    async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<RouteRecord>, DomainError> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| r.key().0 == tenant_id)
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| a.entity.id.cmp(&b.entity.id));
        Ok(out)
    }

    async fn list_routes_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<RouteRecord>, DomainError> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| {
                r.key().0 == tenant_id
                    && gts::parse_resource_id(&r.value().entity.upstream_id) == Some(upstream_id)
            })
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| a.entity.id.cmp(&b.entity.id));
        Ok(out)
    }

    async fn list_routes_for_tenants(
        &self,
        tenant_ids: &[Uuid],
    ) -> Result<Vec<RouteRecord>, DomainError> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| tenant_ids.contains(&r.key().0))
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| a.entity.id.cmp(&b.entity.id));
        Ok(out)
    }

    async fn insert_plugin(&self, record: PluginRecord) -> Result<(), DomainError> {
        let key = plugin_key(&record);
        if self.plugins.contains_key(&key) {
            return Err(DomainError::Conflict {
                detail: "plugin already exists".to_owned(),
            });
        }
        self.plugins.insert(key, record);
        Ok(())
    }

    async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.plugins
            .remove(&(tenant_id, id))
            .map(|_| ())
            .ok_or(DomainError::NotFound)
    }

    async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRecord, DomainError> {
        self.plugins
            .get(&(tenant_id, id))
            .map(|r| r.clone())
            .ok_or(DomainError::NotFound)
    }

    async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<PluginRecord>, DomainError> {
        let mut out: Vec<PluginRecord> = self
            .plugins
            .iter()
            .filter(|r| r.key().0 == tenant_id)
            .map(|r| r.value().clone())
            .collect();
        out.sort_by(|a, b| a.entity.id.cmp(&b.entity.id));
        Ok(out)
    }

    async fn plugin_is_referenced(
        &self,
        tenant_id: Uuid,
        plugin_key_text: &str,
    ) -> Result<bool, DomainError> {
        let referenced = || -> bool {
            for r in &self.upstreams {
                if r.key().0 != tenant_id {
                    continue;
                }
                if let Some(binding) = &r.value().entity.plugins
                    && binding
                        .items
                        .iter()
                        .any(|item| item.as_ref().0 == plugin_key_text)
                {
                    return true;
                }
            }
            for r in &self.routes {
                if r.key().0 != tenant_id {
                    continue;
                }
                if let Some(binding) = &r.value().entity.plugins
                    && binding
                        .items
                        .iter()
                        .any(|item| item.as_ref().0 == plugin_key_text)
                {
                    return true;
                }
            }
            false
        };
        Ok(referenced())
    }
}
