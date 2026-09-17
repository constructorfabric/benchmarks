//! In-memory repository implementations (dashmap-backed).

use std::collections::HashSet;
use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, TenantId, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

fn norm_alias(alias: &str) -> String {
    crate::domain::alias::normalize_alias(alias)
}

/// In-memory upstream repository.
#[derive(Clone, Default)]
pub struct MemoryUpstreamRepository {
    by_id: Arc<DashMap<Uuid, Upstream>>,
    /// (tenant, normalized_alias) → upstream id (UNIQUE constraint).
    by_key: Arc<DashMap<(Uuid, String), Uuid>>,
}

impl UpstreamRepository for MemoryUpstreamRepository {
    fn insert(&self, upstream: Upstream) -> Result<(), DomainError> {
        let alias = upstream.alias.as_deref().unwrap_or_default();
        let key = (upstream.tenant_id, norm_alias(alias));
        if self.by_key.contains_key(&key) {
            return Err(DomainError::AliasViolation(format!(
                "alias '{alias}' already exists for this tenant"
            )));
        }
        self.by_id.insert(upstream.id, upstream.clone());
        self.by_key.insert(key, upstream.id);
        Ok(())
    }

    fn replace(&self, tenant: TenantId, upstream: Upstream) -> Result<(), DomainError> {
        let Some(mut existing) = self.by_id.get_mut(&upstream.id) else {
            return Err(DomainError::not_found("upstream", upstream.id));
        };
        if existing.tenant_id != tenant {
            return Err(DomainError::not_found("upstream", upstream.id));
        }
        // Drop the old key unless the new alias equals the old one.
        let old_key = (
            upstream.tenant_id,
            norm_alias(existing.alias.as_deref().unwrap_or_default()),
        );
        let new_key = (
            upstream.tenant_id,
            norm_alias(upstream.alias.as_deref().unwrap_or_default()),
        );
        if old_key != new_key {
            self.by_key.remove(&old_key);
            if self.by_key.contains_key(&new_key) {
                self.by_key.insert(old_key, upstream.id); // restore
                return Err(DomainError::AliasViolation(format!(
                    "alias '{}' already exists for this tenant",
                    upstream.alias.as_deref().unwrap_or_default()
                )));
            }
        }
        *existing = upstream.clone();
        self.by_key.insert(new_key, upstream.id);
        Ok(())
    }

    fn get_by_id(&self, tenant: TenantId, id: Uuid) -> Option<Upstream> {
        self.by_id
            .get(&id)
            .filter(|u| u.tenant_id == tenant)
            .map(|u| u.clone())
    }

    fn get_by_alias(&self, tenant: TenantId, alias: &str) -> Option<Upstream> {
        let id = self.by_key.get(&(tenant, norm_alias(alias)))?.clone();
        self.by_id.get(&id).map(|u| u.clone())
    }

    fn alias_taken(&self, tenant: TenantId, alias: &str) -> bool {
        self.by_key.contains_key(&(tenant, norm_alias(alias)))
    }

    fn list(&self, tenant: TenantId) -> Vec<Upstream> {
        self.by_id
            .iter()
            .filter(|u| u.tenant_id == tenant)
            .map(|u| u.clone())
            .collect()
    }

    fn delete(&self, tenant: TenantId, id: Uuid) -> Result<(), DomainError> {
        // Grab the alias while the shard guard is held, then release the
        // guard before touching `by_id` again: a DashMap read guard is not
        // reentrant with a later write to the same map, so `remove` while the
        // guard is still live would deadlock.
        let alias = {
            let u = self
                .by_id
                .get(&id)
                .ok_or_else(|| DomainError::not_found("upstream", id))?;
            if u.tenant_id != tenant {
                return Err(DomainError::not_found("upstream", id));
            }
            u.alias.clone()
        };
        self.by_key
            .remove(&(tenant, norm_alias(alias.as_deref().unwrap_or_default())));
        self.by_id.remove(&id);
        Ok(())
    }
}

/// In-memory route repository.
#[derive(Clone, Default)]
pub struct MemoryRouteRepository {
    by_id: Arc<DashMap<Uuid, Route>>,
}

impl RouteRepository for MemoryRouteRepository {
    fn insert(&self, route: Route) -> Result<(), DomainError> {
        self.by_id.insert(route.id, route);
        Ok(())
    }

    fn get_by_id(&self, tenant: TenantId, id: Uuid) -> Option<Route> {
        self.by_id
            .get(&id)
            .filter(|r| r.tenant_id == tenant)
            .map(|r| r.clone())
    }

    fn list(&self, tenant: TenantId) -> Vec<Route> {
        self.by_id
            .iter()
            .filter(|r| r.tenant_id == tenant)
            .map(|r| r.clone())
            .collect()
    }

    fn list_for_upstream(&self, tenant: TenantId, upstream_id: Uuid) -> Vec<Route> {
        self.by_id
            .iter()
            .filter(|r| r.tenant_id == tenant && r.upstream_id == upstream_id)
            .map(|r| r.clone())
            .collect()
    }

    fn replace(&self, tenant: TenantId, route: Route) -> Result<(), DomainError> {
        let Some(mut existing) = self.by_id.get_mut(&route.id) else {
            return Err(DomainError::not_found("route", route.id));
        };
        if existing.tenant_id != tenant {
            return Err(DomainError::not_found("route", route.id));
        }
        *existing = route;
        Ok(())
    }

    fn delete(&self, tenant: TenantId, id: Uuid) -> Result<(), DomainError> {
        // Scoped check so the read guard drops before `by_id.remove` (see
        // UpstreamRepository::delete for the deadlock rationale).
        {
            let r = self
                .by_id
                .get(&id)
                .ok_or_else(|| DomainError::not_found("route", id))?;
            if r.tenant_id != tenant {
                return Err(DomainError::not_found("route", id));
            }
        }
        self.by_id.remove(&id);
        Ok(())
    }

    fn any_route_for_upstream(&self, upstream_id: Uuid) -> bool {
        self.by_id
            .iter()
            .any(|r| r.upstream_id == upstream_id)
    }
}

/// In-memory custom-plugin repository.
#[derive(Clone, Default)]
pub struct MemoryPluginRepository {
    by_id: Arc<DashMap<Uuid, Plugin>>,
    by_name: Arc<DashMap<(TenantId, String), Uuid>>,
}

impl PluginRepository for MemoryPluginRepository {
    fn insert(&self, plugin: Plugin) -> Result<(), DomainError> {
        let key = (plugin.tenant_id, plugin.name.to_ascii_lowercase());
        if self.by_name.contains_key(&key) {
            return Err(DomainError::AliasViolation(format!(
                "a plugin named '{}' already exists in this tenant",
                plugin.name
            )));
        }
        self.by_id.insert(plugin.id, plugin.clone());
        self.by_name.insert(key, plugin.id);
        Ok(())
    }

    fn get_by_id(&self, tenant: TenantId, id: Uuid) -> Option<Plugin> {
        self.by_id
            .get(&id)
            .filter(|p| p.tenant_id == tenant)
            .map(|p| p.clone())
    }

    fn get_by_name(&self, tenant: TenantId, name: &str) -> Option<Plugin> {
        let id = self.by_name.get(&(tenant, name.to_ascii_lowercase()))?.clone();
        self.by_id.get(&id).map(|p| p.clone())
    }

    fn list(&self, tenant: TenantId) -> Vec<Plugin> {
        self.by_id
            .iter()
            .filter(|p| p.tenant_id == tenant)
            .map(|p| p.clone())
            .collect()
    }

    fn delete(&self, tenant: TenantId, id: Uuid) -> Result<(), DomainError> {
        // Capture the name while the guard is held so the guard is released
        // before `by_id.remove` (see UpstreamRepository::delete).
        let name = {
            let p = self
                .by_id
                .get(&id)
                .ok_or_else(|| DomainError::not_found("plugin", id))?;
            if p.tenant_id != tenant {
                return Err(DomainError::not_found("plugin", id));
            }
            p.name.clone()
        };
        self.by_name.remove(&(tenant, name.to_ascii_lowercase()));
        self.by_id.remove(&id);
        Ok(())
    }

    fn mark_gc_eligible(
        &self,
        tenant: TenantId,
        id: Uuid,
        eligible_at: u64,
    ) -> Result<(), DomainError> {
        let Some(mut p) = self.by_id.get_mut(&id) else {
            return Err(DomainError::not_found("plugin", id));
        };
        if p.tenant_id != tenant {
            return Err(DomainError::not_found("plugin", id));
        }
        p.gc_eligible_at = Some(eligible_at);
        Ok(())
    }
}

/// Names of all upstreams and route aliases referenced by a tenant (used by
/// GC bookkeeping in tests).
#[allow(dead_code)]
pub(crate) fn all_route_upstream_ids(routes: &[Route]) -> HashSet<Uuid> {
    routes.iter().map(|r| r.upstream_id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, MatchConfig, Protocol, ServerConfig};

    fn test_upstream(tenant: Uuid, alias: &str, id: Uuid) -> Upstream {
        Upstream {
            id,
            tenant_id: tenant,
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "http".into(),
                    host: "example.com".into(),
                    port: 80,
                }],
            },
            protocol: Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            bound: false,
        }
    }

    #[test]
    fn upstream_crud_and_case_insensitive_alias() {
        let tenant = Uuid::new_v4();
        let repo = MemoryUpstreamRepository::default();
        let id = Uuid::new_v4();
        repo.insert(test_upstream(tenant, "api.example.com", id)).unwrap();

        // Case-insensitive resolution.
        assert!(repo.get_by_alias(tenant, "API.EXAMPLE.COM.").is_some());
        // Tenant scoping.
        assert!(repo.get_by_alias(Uuid::new_v4(), "api.example.com").is_none());
        assert!(repo.get_by_id(Uuid::new_v4(), id).is_none());
        // Uniqueness.
        assert!(repo.insert(test_upstream(tenant, "api.Example.com", Uuid::new_v4())).is_err());

        // Replace keeps alias.
        let mut u = test_upstream(tenant, "api.example.com", id);
        u.enabled = false;
        repo.replace(tenant, u.clone()).unwrap();
        assert!(!repo.get_by_id(tenant, id).unwrap().enabled);

        // Delete.
        repo.delete(tenant, id).unwrap();
        assert!(repo.get_by_id(tenant, id).is_none());
        assert!(repo.delete(tenant, id).is_err());
    }

    #[test]
    fn route_and_plugin_scoping() {
        let tenant = Uuid::new_v4();
        let routes = MemoryRouteRepository::default();
        let up_id = Uuid::new_v4();
        let r1 = Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: up_id,
            enabled: true,
            tags: Vec::new(),
            priority: 0,
            match_: MatchConfig::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        };
        routes.insert(r1.clone()).unwrap();
        assert_eq!(routes.list_for_upstream(tenant, up_id).len(), 1);
        assert!(routes.any_route_for_upstream(up_id));
        assert_eq!(routes.list_for_upstream(Uuid::new_v4(), up_id).len(), 0);

        let plugins = MemoryPluginRepository::default();
        let p = Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            name: "my-guard".into(),
            description: None,
            kind: crate::domain::model::PluginKind::Guard,
            config_schema: serde_json::Value::Null,
            source_code: "def main(): pass".into(),
            gc_eligible_at: None,
        };
        plugins.insert(p.clone()).unwrap();
        assert!(plugins.get_by_name(tenant, "MY-GUARD").is_some());
        assert!(plugins
            .insert(Plugin { name: "My-Guard".into(), ..p.clone() })
            .is_err());
        plugins.mark_gc_eligible(tenant, p.id, 123).unwrap();
        assert_eq!(plugins.get_by_id(tenant, p.id).unwrap().gc_eligible_at, Some(123));
    }
}
