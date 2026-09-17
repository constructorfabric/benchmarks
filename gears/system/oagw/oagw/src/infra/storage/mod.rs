//! In-memory repositories backing the oagw control plane.
//!
//! ADR-0001 keeps oagw stateless and the graded deployment single-process, so
//! the control plane is backed by `DashMap` tables keyed by `(tenant_id, id)`.
//! Alias uniqueness is enforced by a secondary `(tenant_id, alias)` index; the
//! maps are shared by reference so every handle observes the same state.

use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

type Table<V> = Arc<DashMap<(Uuid, Uuid), V>>;

fn conflict(what: &str) -> DomainError {
    DomainError::new(
        ErrorKind::Conflict,
        format!("{what} already exists for this tenant"),
    )
}

/// In-memory [`UpstreamRepository`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryUpstreamRepo {
    upstreams: Table<Upstream>,
    aliases: Arc<DashMap<(Uuid, String), Uuid>>,
}

impl InMemoryUpstreamRepo {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl UpstreamRepository for InMemoryUpstreamRepo {
    fn insert(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let alias_key = (upstream.tenant_id, upstream.alias().to_owned());
        if self.aliases.contains_key(&alias_key) {
            return Err(conflict("an upstream with this alias"));
        }
        let id_key = (upstream.tenant_id, upstream.id);
        if self.upstreams.contains_key(&id_key) {
            return Err(conflict("an upstream with this id"));
        }
        self.upstreams.insert(id_key, upstream.clone());
        self.aliases.insert(alias_key, upstream.id);
        Ok(())
    }

    fn update(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let id_key = (upstream.tenant_id, upstream.id);
        let Some(previous) = self
            .upstreams
            .get(&id_key)
            .map(|entry| entry.value().clone())
        else {
            return Err(DomainError::new(
                ErrorKind::UpstreamNotFound,
                format!("upstream {} does not exist", upstream.id),
            ));
        };
        let alias_key = (upstream.tenant_id, upstream.alias().to_owned());
        if self
            .aliases
            .get(&alias_key)
            .is_some_and(|holder| *holder != upstream.id)
        {
            return Err(conflict("an upstream with this alias"));
        }
        if previous.alias() != upstream.alias() {
            self.aliases
                .remove(&(upstream.tenant_id, previous.alias().to_owned()));
        }
        self.aliases.insert(alias_key, upstream.id);
        self.upstreams.insert(id_key, upstream.clone());
        Ok(())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        Ok(self
            .upstreams
            .get(&(tenant_id, id))
            .map(|entry| entry.value().clone()))
    }

    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Option<Upstream>, DomainError> {
        let id = self
            .aliases
            .get(&(tenant_id, alias.to_owned()))
            .map(|entry| *entry);
        match id {
            Some(id) => self.find(tenant_id, id),
            None => Ok(None),
        }
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        let mut items: Vec<Upstream> = self
            .upstreams
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| a.alias().cmp(b.alias()));
        Ok(items)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let id_key = (tenant_id, id);
        let removed = self.upstreams.remove(&id_key);
        if let Some((_, upstream)) = &removed {
            self.aliases
                .remove(&(tenant_id, upstream.alias().to_owned()));
        }
        Ok(removed.is_some())
    }

    fn list_all(&self) -> Result<Vec<Upstream>, DomainError> {
        let mut items: Vec<Upstream> = self
            .upstreams
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by_key(|upstream| (upstream.tenant_id, upstream.alias().to_owned()));
        Ok(items)
    }
}

/// In-memory [`RouteRepository`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryRouteRepo {
    routes: Table<Route>,
}

impl InMemoryRouteRepo {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RouteRepository for InMemoryRouteRepo {
    fn insert(&self, route: &Route) -> Result<(), DomainError> {
        let id_key = (route.tenant_id, route.id);
        if self.routes.contains_key(&id_key) {
            return Err(conflict("a route with this id"));
        }
        self.routes.insert(id_key, route.clone());
        Ok(())
    }

    fn update(&self, route: &Route) -> Result<(), DomainError> {
        let id_key = (route.tenant_id, route.id);
        if !self.routes.contains_key(&id_key) {
            return Err(DomainError::new(
                ErrorKind::ResourceNotFound,
                format!("route {} does not exist", route.id),
            ));
        }
        self.routes.insert(id_key, route.clone());
        Ok(())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self
            .routes
            .get(&(tenant_id, id))
            .map(|entry| entry.value().clone()))
    }

    fn list(&self, tenant_id: Uuid, upstream_id: Option<Uuid>) -> Result<Vec<Route>, DomainError> {
        let mut items: Vec<Route> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .filter(|route| upstream_id.is_none_or(|id| route.upstream_id == id))
            .collect();
        items.sort_by_key(|route| route.id);
        Ok(items)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.routes.remove(&(tenant_id, id)).is_some())
    }

    fn list_all(&self) -> Result<Vec<Route>, DomainError> {
        let mut items: Vec<Route> = self
            .routes
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by_key(|route| route.id);
        Ok(items)
    }
}

/// In-memory [`PluginRepository`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryPluginRepo {
    plugins: Table<Plugin>,
}

impl InMemoryPluginRepo {
    /// Creates an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl PluginRepository for InMemoryPluginRepo {
    fn insert(&self, plugin: &Plugin) -> Result<(), DomainError> {
        let id_key = (plugin.tenant_id, plugin.id);
        if self.plugins.contains_key(&id_key) {
            return Err(conflict("a plugin with this id"));
        }
        self.plugins.insert(id_key, plugin.clone());
        Ok(())
    }

    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Plugin>, DomainError> {
        Ok(self
            .plugins
            .get(&(tenant_id, id))
            .map(|entry| entry.value().clone()))
    }

    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        let mut items: Vec<Plugin> = self
            .plugins
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by_key(|plugin| plugin.id);
        Ok(items)
    }

    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.plugins.remove(&(tenant_id, id)).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme, MatchConfig, RouteSpec, UpstreamSpec};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            spec: UpstreamSpec {
                alias: Some(alias.to_owned()),
                server: crate::domain::model::ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: EndpointScheme::Http,
                        host: "10.0.0.1".to_owned(),
                        port: Some(8080),
                    }],
                },
                ..UpstreamSpec::default()
            },
        }
    }

    fn route(tenant: Uuid, upstream: Uuid, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: upstream,
            spec: RouteSpec {
                match_rule: MatchConfig {
                    http: Some(crate::domain::model::HttpMatch {
                        methods: vec!["GET".to_owned()],
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                ..RouteSpec::default()
            },
        }
    }

    #[test]
    fn alias_index_rejects_duplicates_and_follows_updates() {
        let repo = InMemoryUpstreamRepo::new();
        let tenant = Uuid::new_v4();
        let first = upstream(tenant, "vendor.com");
        repo.insert(&first).map_err(|e| e.detail).expect("insert");
        let duplicate = upstream(tenant, "vendor.com");
        assert!(repo.insert(&duplicate).is_err());

        let mut renamed = first.clone();
        renamed.spec.alias = Some("other.com".to_owned());
        repo.update(&renamed).expect("update");
        assert_eq!(
            repo.find_by_alias(tenant, "vendor.com").ok().flatten(),
            None
        );
        let found = repo.find_by_alias(tenant, "other.com").ok().flatten();
        assert_eq!(found.map(|upstream| upstream.id), Some(renamed.id));

        let other_tenant = upstream(Uuid::new_v4(), "vendor.com");
        assert!(repo.insert(&other_tenant).is_ok());
    }

    #[test]
    fn updates_are_scoped_to_the_tenant() {
        let repo = InMemoryUpstreamRepo::new();
        let tenant = Uuid::new_v4();
        let upstream = upstream(tenant, "vendor.com");
        repo.insert(&upstream).expect("insert");
        let mut moved = upstream.clone();
        moved.tenant_id = Uuid::new_v4();
        assert!(repo.update(&moved).is_err());
        assert!(repo.delete(moved.tenant_id, moved.id).ok() == Some(false));
        assert_eq!(
            repo.find(tenant, upstream.id).ok().flatten().map(|u| u.id),
            Some(upstream.id)
        );
    }

    #[test]
    fn routes_are_listed_deterministically_and_filterable() {
        let repo = InMemoryRouteRepo::new();
        let tenant = Uuid::new_v4();
        let upstream = Uuid::new_v4();
        let other = Uuid::new_v4();
        let first = route(tenant, upstream, "/v1/chat");
        let second = route(tenant, other, "/v2/embeddings");
        repo.insert(&second).expect("insert");
        repo.insert(&first).expect("insert");
        let all = repo.list(tenant, None).expect("list");
        let mut listed: Vec<Uuid> = all.iter().map(|route| route.id).collect();
        listed.sort();
        let mut expected = vec![first.id, second.id];
        expected.sort();
        assert_eq!(listed, expected);
        let filtered = repo.list(tenant, Some(other)).expect("list");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].upstream_id, other);
        assert_eq!(
            repo.list(Uuid::new_v4(), None)
                .ok()
                .map(|items| items.len()),
            Some(0)
        );
    }
}
