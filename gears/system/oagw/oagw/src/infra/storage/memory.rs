//! In-memory repositories.
//!
//! Phase-1 persistence: `Arc`-shared, lock-protected maps that keep insertion
//! order so list endpoints are deterministic. The same trait contracts
//! ([`crate::domain::repo`]) are what a SeaORM-backed implementation satisfies
//! in a later phase.

use std::collections::BTreeMap;
use std::sync::RwLock;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{Route, Upstream};
use crate::domain::repo::{Repositories, RouteRepository, UpstreamRepository};

/// A collection that keeps insertion order and is addressable by id.
#[derive(Debug)]
struct OrderedStore<T> {
    order: RwLock<Vec<Uuid>>,
    items: RwLock<BTreeMap<Uuid, T>>,
}

impl<T> Default for OrderedStore<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> OrderedStore<T> {
    fn new() -> Self {
        Self {
            order: RwLock::new(Vec::new()),
            items: RwLock::new(BTreeMap::new()),
        }
    }
}

impl<T: Clone> OrderedStore<T> {
    fn get(&self, id: Uuid) -> Option<T> {
        self.items.read().expect("store lock").get(&id).cloned()
    }

    fn put(&self, id: Uuid, value: T) {
        {
            let mut items = self.items.write().expect("store lock");
            if items.insert(id, value).is_none() {
                self.order.write().expect("store lock").push(id);
            }
        }
    }

    fn remove(&self, id: Uuid) -> Option<T> {
        let removed = self.items.write().expect("store lock").remove(&id);
        if removed.is_some() {
            self.order.write().expect("store lock").retain(|k| *k != id);
        }
        removed
    }

    fn all(&self) -> Vec<T> {
        let items = self.items.read().expect("store lock");
        let order = self.order.read().expect("store lock");
        order.iter().filter_map(|id| items.get(id).cloned()).collect()
    }

    fn retain(&self, keep: impl Fn(&T) -> bool) -> usize {
        let mut items = self.items.write().expect("store lock");
        let mut order = self.order.write().expect("store lock");
        let before = items.len();
        items.retain(|_, value| keep(value));
        order.retain(|id| items.contains_key(id));
        before - items.len()
    }
}

/// In-memory upstream + route storage.
#[derive(Debug, Default)]
pub struct InMemoryRepositories {
    upstreams: OrderedStore<Upstream>,
    routes: OrderedStore<Route>,
}

impl InMemoryRepositories {
    /// Empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }


    /// Repository bundle view of this store.
    #[must_use]
    pub fn into_repos(self) -> Repositories {
        let store = std::sync::Arc::new(self);
        Repositories {
            upstreams: store.clone(),
            routes: store,
        }
    }

    /// Live (shared) repository bundle over a new empty store.
    #[must_use]
    pub fn shared() -> Repositories {
        Self::new().into_repos()
    }
}

impl UpstreamRepository for InMemoryRepositories {
    fn insert(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        if let Some(existing) = self.upstreams.get(upstream.id)
            && existing.tenant_id == upstream.tenant_id
            && existing.alias == upstream.alias
        {
            return Err(DomainError::AliasConflict {
                alias: upstream.alias.clone(),
                existing_id: existing.id,
            });
        }
        self.upstreams.put(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        Ok(self
            .upstreams
            .get(id)
            .filter(|u| u.tenant_id == tenant_id))
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Upstream>, DomainError> {
        let wanted = alias.trim().trim_end_matches('.').to_ascii_lowercase();
        Ok(self
            .upstreams
            .all()
            .into_iter()
            .find(|u| u.tenant_id == tenant_id && u.alias == wanted))
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        Ok(self
            .upstreams
            .all()
            .into_iter()
            .filter(|u| u.tenant_id == tenant_id)
            .collect())
    }

    fn replace(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        let existing = self.upstreams.get(upstream.id).ok_or_else(|| {
            DomainError::NotFound(format!("no upstream with id {}", upstream.id))
        })?;
        if existing.tenant_id != upstream.tenant_id {
            return Err(DomainError::NotFound(format!(
                "no upstream with id {}",
                upstream.id
            )));
        }
        self.upstreams.put(upstream.id, upstream.clone());
        Ok(upstream)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let existing = self.upstreams.get(id);
        match existing {
            Some(u) if u.tenant_id == tenant_id => Ok(self.upstreams.remove(id).is_some()),
            _ => Ok(false),
        }
    }

    fn alias_owner(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Uuid>, DomainError> {
        Ok(self
            .find_by_alias(tenant_id, alias)?
            .map(|u| u.id))
    }
}

impl RouteRepository for InMemoryRepositories {
    fn insert(&self, route: Route) -> Result<Route, DomainError> {
        self.routes.put(route.id, route.clone());
        Ok(route)
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self.routes.get(id).filter(|r| r.tenant_id == tenant_id))
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        Ok(self
            .routes
            .all()
            .into_iter()
            .filter(|r| r.tenant_id == tenant_id)
            .collect())
    }

    fn list_by_upstream(&self, upstream_id: Uuid) -> Result<Vec<Route>, DomainError> {
        Ok(self
            .routes
            .all()
            .into_iter()
            .filter(|r| r.upstream_id == upstream_id)
            .collect())
    }

    fn replace(&self, route: Route) -> Result<Route, DomainError> {
        let existing =
            self.routes
                .get(route.id)
                .ok_or_else(|| DomainError::NotFound(format!("no route with id {}", route.id)))?;
        if existing.tenant_id != route.tenant_id {
            return Err(DomainError::NotFound(format!(
                "no route with id {}",
                route.id
            )));
        }
        self.routes.put(route.id, route.clone());
        Ok(route)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        match self.routes.get(id) {
            Some(route) if route.tenant_id == tenant_id => Ok(self.routes.remove(id).is_some()),
            _ => Ok(false),
        }
    }

    fn delete_by_upstream(&self, upstream_id: Uuid) -> Result<usize, DomainError> {
        Ok(self.routes.retain(|r| r.upstream_id != upstream_id))
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::alias::resolve_alias_for_spec;
    use crate::domain::models::{Endpoint, EndpointScheme, UpstreamSpec};

    fn upstream(alias: &str, host: &str, tenant_id: Uuid) -> Upstream {
    let _ = alias;
        let spec = UpstreamSpec {
            server: crate::domain::models::ServerConfig {
                endpoints: vec![Endpoint::new(EndpointScheme::Https, host, 443)],
            },
            ..UpstreamSpec::default()
        };
        let (_, derived) = resolve_alias_for_spec(
            spec.server.endpoints.as_slice(),
            spec.alias.as_deref(),
        )
        .expect("derivable");
        Upstream::from_spec(spec, Uuid::new_v4(), tenant_id, derived)
    }

    #[test]
    fn ordered_store_keeps_insertion_order() {
        let store: OrderedStore<u32> = OrderedStore::new();
        store.put(Uuid::from_u128(3), 30);
        store.put(Uuid::from_u128(1), 10);
        store.put(Uuid::from_u128(2), 20);
        assert_eq!(store.all(), vec![30, 10, 20]);
        store.remove(Uuid::from_u128(1));
        assert_eq!(store.all(), vec![30, 20]);
        assert_eq!(store.retain(|v| *v == 20), 1);
        assert_eq!(store.all(), vec![20]);
    }

    #[test]
    fn upstreams_are_scoped_per_tenant() {
        let repos = InMemoryRepositories::shared();
        let tenant_a = Uuid::new_v4();
        let tenant_b = Uuid::new_v4();
        let upstream = upstream("api.openai.com", "api.openai.com", tenant_a);
        let id = upstream.id;
        repos.upstreams.insert(upstream).expect("insert");

        assert!(repos.upstreams.find(tenant_a, id).is_ok());
        assert!(repos.upstreams.find(tenant_b, id).unwrap().is_none());
        assert!(repos.upstreams.find_by_alias(tenant_a, "API.OpenAI.COM.").unwrap().is_some());
        assert!(repos.upstreams.find_by_alias(tenant_b, "api.openai.com").unwrap().is_none());
        assert_eq!(
            repos.upstreams.alias_owner(tenant_a, "api.openai.com").unwrap(),
            Some(id)
        );
        assert_eq!(repos.upstreams.alias_owner(tenant_b, "api.openai.com").unwrap(), None);
    }

    #[test]
    fn deleting_an_upstream_is_scoped() {
        let repos = InMemoryRepositories::shared();
        let tenant = Uuid::new_v4();
        let other = Uuid::new_v4();
        let u = upstream("a.example", "a.example", tenant);
        let id = u.id;
        repos.upstreams.insert(u).expect("insert");
        assert!(!repos.upstreams.delete(other, id).unwrap(), "another tenant's delete is a no-op");
        assert!(repos.upstreams.find(tenant, id).unwrap().is_some());
        assert!(repos.upstreams.delete(tenant, id).unwrap());
        assert!(repos.upstreams.find(tenant, id).unwrap().is_none());
    }

    #[test]
    fn routes_are_indexed_by_upstream() {
        let repos = InMemoryRepositories::shared();
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            tags: Vec::new(),
            match_rules: crate::domain::models::MatchConfig::default(),
            plugins: Default::default(),
            rate_limit: None,
        };
        repos.routes.insert(route.clone()).expect("insert");
        assert_eq!(repos.routes.list_by_upstream(upstream_id).unwrap().len(), 1);
        assert!(repos.routes.delete(tenant, route.id).unwrap());
        assert_eq!(repos.routes.delete_by_upstream(upstream_id).unwrap(), 0);
    }

    #[test]
    fn replace_keeps_tenant_scoping() {
        let repos = InMemoryRepositories::shared();
        let tenant = Uuid::new_v4();
        let u = upstream("a.example", "a.example", tenant);
        repos.upstreams.insert(u.clone()).expect("insert");
        let mut renamed = u.clone();
        renamed.alias = "b.example".to_owned();
        assert!(repos.upstreams.replace(renamed).is_ok());
        let foreign = {
            let mut u = u.clone();
            u.tenant_id = Uuid::new_v4();
            u
        };
        assert!(repos.upstreams.replace(foreign).is_err());
    }
}
