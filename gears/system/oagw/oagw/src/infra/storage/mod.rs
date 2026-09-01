//! In-memory repository implementations for the OAGW control plane.
//!
//! OAGW has no database (DEV/bench note): state lives in process memory,
//! tenant-keyed, with the same uniqueness semantics as the DB constraints
//! from DESIGN §3.6:
//! - upstream: `UNIQUE (tenant_id, alias)`
//! - route: FK on `upstream_id` (cascade delete)
//! - plugin: `UNIQUE (tenant_id, name)`

use std::collections::HashMap;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RepoConflict, RouteRepository, UpstreamRepository};
use crate::domain::services::management::{normalize_path, routes_conflict};

/// Bundled in-memory repositories.
#[derive(Clone, Default)]
pub struct MemoryRepos {
    /// Upstream store.
    pub upstreams: Arc<MemoryUpstreamRepository>,
    /// Route store.
    pub routes: Arc<MemoryRouteRepository>,
    /// Plugin store.
    pub plugins: Arc<MemoryPluginRepository>,
}

impl MemoryRepos {
    /// Create an empty repository bundle.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn key(tenant_id: Uuid, id: Uuid) -> (Uuid, Uuid) {
    (tenant_id, id)
}

/// In-memory upstream repository.
#[derive(Debug, Default)]
pub struct MemoryUpstreamRepository {
    inner: DashMap<(Uuid, Uuid), Upstream>,
    /// Serializes uniqueness check + insert so the `UNIQUE (tenant, alias)`
    /// check is atomic (no interleaved inserts can slip past it).
    guard: parking_lot::Mutex<()>,
}

impl MemoryUpstreamRepository {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl UpstreamRepository for MemoryUpstreamRepository {
    fn insert(&self, upstream: Upstream) -> Result<(), RepoConflict> {
        let tenant_id = upstream.tenant_id.unwrap_or_default();
        let id = upstream.id.unwrap_or_default();
        let alias = upstream.alias.as_deref();
        let _guard = self.guard.lock();
        if let Some(alias) = alias
            && self.inner.iter().any(|e| {
                e.value().tenant_id == Some(tenant_id) && e.value().alias.as_deref() == Some(alias)
            })
        {
            return Err(RepoConflict::DuplicateAlias);
        }
        self.inner.insert(key(tenant_id, id), upstream);
        Ok(())
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.inner.get(&key(tenant_id, id)).map(|e| e.clone())
    }

    fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        self.inner
            .iter()
            .find(|e| {
                e.value().tenant_id == Some(tenant_id) && e.value().alias.as_deref() == Some(alias)
            })
            .map(|e| e.value().clone())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut out: Vec<Upstream> = self
            .inner
            .iter()
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .collect();
        out.sort_by_key(|u| u.alias.clone().unwrap_or_default());
        out
    }

    fn replace(&self, upstream: Upstream) {
        let tenant_id = upstream.tenant_id.unwrap_or_default();
        let id = upstream.id.unwrap_or_default();
        let _guard = self.guard.lock();
        // Drop any other row of this tenant that collides on the new alias
        // (should not happen — enforced at the service layer).
        self.inner.retain(|k, v| {
            k.1 == id || v.tenant_id != Some(tenant_id) || v.alias != upstream.alias
        });
        self.inner.insert(key(tenant_id, id), upstream);
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        self.inner.remove(&key(tenant_id, id)).is_some()
    }
}

/// In-memory route repository.
#[derive(Debug, Default)]
pub struct MemoryRouteRepository {
    inner: DashMap<(Uuid, Uuid), Route>,
    /// Serializes the conflict check + insert (atomic like the DB
    /// `UNIQUE (upstream_id, path, method)` semantics).
    guard: parking_lot::Mutex<()>,
}

impl MemoryRouteRepository {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RouteRepository for MemoryRouteRepository {
    fn insert(&self, route: Route) -> Result<(), RepoConflict> {
        let tenant_id = route.tenant_id.unwrap_or_default();
        let id = route.id.unwrap_or_default();
        let _guard = self.guard.lock();
        if route.enabled {
            let conflict = self
                .inner
                .iter()
                .filter(|e| e.value().tenant_id == Some(tenant_id))
                .map(|e| e.value().clone())
                .any(|existing| routes_conflict(&existing, &route));
            if conflict {
                return Err(RepoConflict::DuplicateAlias);
            }
        }
        self.inner.insert(key(tenant_id, id), route);
        Ok(())
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.inner.get(&key(tenant_id, id)).map(|e| e.clone())
    }

    fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Vec<Route> {
        let mut out: Vec<Route> = self
            .inner
            .iter()
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .filter(|r| upstream_id.is_none_or(|u| r.upstream_id == u))
            .collect();
        out.sort_by_key(|r| {
            r.r#match
                .as_http()
                .map(|h| (normalize_path(&h.path), h.methods.join(",")))
                .unwrap_or_default()
        });
        out
    }

    fn replace(&self, route: Route) {
        let tenant_id = route.tenant_id.unwrap_or_default();
        let id = route.id.unwrap_or_default();
        self.inner.insert(key(tenant_id, id), route);
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        self.inner.remove(&key(tenant_id, id)).is_some()
    }

    fn delete_by_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) {
        self.inner.retain(|k, v| {
            k.0 != tenant_id || v.tenant_id != Some(tenant_id) || v.upstream_id != upstream_id
        });
    }
}

/// In-memory custom plugin repository.
#[derive(Debug, Default)]
pub struct MemoryPluginRepository {
    inner: DashMap<(Uuid, Uuid), Plugin>,
    /// `(tenant_id, name)` → id index for the unique constraint.
    names: Mutex<HashMap<(Uuid, String), Uuid>>,
}

impl MemoryPluginRepository {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PluginRepository for MemoryPluginRepository {
    fn insert(&self, plugin: Plugin) -> Result<(), RepoConflict> {
        let tenant_id = plugin.tenant_id.unwrap_or_default();
        let id = plugin.id.unwrap_or_default();
        let mut names = self.names.lock();
        if names.get(&(tenant_id, plugin.name.clone())).is_some() {
            return Err(RepoConflict::DuplicateName);
        }
        self.inner.insert(key(tenant_id, id), plugin.clone());
        names.insert((tenant_id, plugin.name), id);
        Ok(())
    }

    fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        self.inner.get(&key(tenant_id, id)).map(|e| e.clone())
    }

    fn list(&self, tenant_id: Uuid) -> Vec<Plugin> {
        let mut out: Vec<Plugin> = self
            .inner
            .iter()
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let removed = self.inner.remove(&key(tenant_id, id));
        if let Some((_, p)) = &removed {
            self.names.lock().remove(&(tenant_id, p.name.clone()));
        }
        removed.is_some()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::dto::Endpoint;
    use crate::domain::dto::EndpointScheme;

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            tenant_id: Some(tenant),
            alias: Some(alias.to_owned()),
            server: crate::domain::dto::ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.example.com".into(),
                    port: 443,
                }],
            },
            ..Upstream::default()
        }
    }

    #[test]
    fn upstream_unique_alias_per_tenant() {
        let repo = MemoryUpstreamRepository::new();
        let t1 = Uuid::new_v4();
        let t2 = Uuid::new_v4();
        assert!(repo.insert(upstream(t1, "api.example.com")).is_ok());
        assert!(matches!(
            repo.insert(upstream(t1, "api.example.com")),
            Err(RepoConflict::DuplicateAlias)
        ));
        // Same alias in a different tenant is fine.
        assert!(repo.insert(upstream(t2, "api.example.com")).is_ok());
        assert_eq!(repo.list(t1).len(), 1);
    }

    #[test]
    fn route_cascade_delete_by_upstream() {
        let repo = MemoryRouteRepository::new();
        let t1 = Uuid::new_v4();
        let up = Uuid::new_v4();
        let r1 = Route {
            id: Some(Uuid::new_v4()),
            tenant_id: Some(t1),
            upstream_id: up,
            ..Route::default()
        };
        repo.insert(r1).unwrap();
        repo.delete_by_upstream(t1, up);
        assert!(repo.list(t1, None).is_empty());
    }
}
