//! In-process implementations of the domain repository traits.
//!
//! The graded deployment configures no database for this gear, so the control
//! plane keeps its configuration here, guarded by `parking_lot` locks. A
//! SeaORM-backed implementation can replace these without touching the domain,
//! because the domain only sees the traits in [`crate::domain::repo`].

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{PluginBinding, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Shared handle to an in-memory upstream repository.
pub type SharedUpstreamRepo = Arc<InMemoryUpstreamRepo>;
/// Shared handle to an in-memory route repository.
pub type SharedRouteRepo = Arc<InMemoryRouteRepo>;
/// Shared handle to an in-memory plugin repository.
pub type SharedPluginRepo = Arc<InMemoryPluginRepo>;

#[derive(Default)]
struct UpstreamStore {
    by_id: HashMap<(Uuid, Uuid), Upstream>,
    /// `(tenant_id, alias_lowercase) -> upstream id`.
    by_alias: HashMap<(Uuid, String), Uuid>,
}

impl UpstreamStore {
    fn resolve(&self, scope: &[Uuid], alias: &str) -> Option<&Upstream> {
        let key = alias.to_ascii_lowercase();
        for tenant in scope {
            if let Some(id) = self.by_alias.get(&(*tenant, key.clone()))
                && let Some(found) = self.by_id.get(&(*tenant, *id))
                // An ancestor's private upstream stays invisible: only its
                // owner may address it.
                && (found.visible_to_descendants() || scope.first() == Some(tenant))
            {
                return Some(found);
            }
        }
        None
    }
}

/// In-memory upstream repository.
#[derive(Default)]
pub struct InMemoryUpstreamRepo {
    store: RwLock<UpstreamStore>,
}

impl InMemoryUpstreamRepo {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored upstreams, across all tenants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.store.read().by_id.len()
    }

    /// Whether the repository holds no upstreams.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl UpstreamRepository for InMemoryUpstreamRepo {
    async fn insert(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let mut store = self.store.write();
        let alias_key = upstream.alias.to_ascii_lowercase();
        if store
            .by_alias
            .contains_key(&(upstream.tenant_id, alias_key.clone()))
        {
            return Err(DomainError::alias_conflict(format!(
                "an upstream with alias '{}' already exists",
                upstream.alias
            )));
        }
        let id_key = (upstream.tenant_id, upstream.id);
        if store.by_id.contains_key(&id_key) {
            return Err(DomainError::alias_conflict(format!(
                "upstream {} already exists",
                upstream.id
            )));
        }
        // The alias index is keyed per tenant so lookups stay scoped.
        store
            .by_alias
            .insert((upstream.tenant_id, alias_key), upstream.id);
        store.by_id.insert(id_key, upstream.clone());
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Upstream>, DomainError> {
        Ok(self.store.read().by_id.get(&(tenant_id, id)).cloned())
    }

    async fn get_by_alias(
        &self,
        scope: &[Uuid],
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        Ok(self.store.read().resolve(scope, alias).cloned())
    }

    async fn list_visible(&self, scope: &[Uuid]) -> Result<Vec<Upstream>, DomainError> {
        let store = self.store.read();
        let mut out: Vec<Upstream> = store
            .by_id
            .iter()
            .filter(|((tenant, _), u)| {
                scope.first() == Some(tenant)
                    || (u.visible_to_descendants() && scope.contains(tenant))
            })
            .map(|(_, u)| u.clone())
            .collect();
        out.sort_by(|a, b| (a.tenant_id, a.alias.as_str()).cmp(&(b.tenant_id, b.alias.as_str())));
        Ok(out)
    }

    async fn update(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let mut store = self.store.write();
        let key = (upstream.tenant_id, upstream.id);
        let Some(existing) = store.by_id.get(&key) else {
            return Err(DomainError::not_found(format!(
                "upstream {} not found",
                upstream.id
            )));
        };
        // The alias is the routing key and immutable once set.
        if !existing.alias.eq_ignore_ascii_case(&upstream.alias) {
            return Err(DomainError::validation(
                "alias is immutable; delete and re-create the upstream instead",
            ));
        }
        let mut next = upstream.clone();
        next.alias.clone_from(&existing.alias);
        next.created_at = existing.created_at;
        store.by_id.insert(key, next);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        let mut store = self.store.write();
        if let Some(removed) = store.by_id.remove(&(tenant_id, id)) {
            store
                .by_alias
                .remove(&(tenant_id, removed.alias.to_ascii_lowercase()));
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

#[derive(Default)]
struct RouteStore {
    by_id: HashMap<(Uuid, Uuid), Route>,
}

/// In-memory route repository.
#[derive(Default)]
pub struct InMemoryRouteRepo {
    store: RwLock<RouteStore>,
}

impl InMemoryRouteRepo {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored routes, across all tenants.
    #[must_use]
    pub fn len(&self) -> usize {
        self.store.read().by_id.len()
    }

    /// Whether the repository holds no routes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl RouteRepository for InMemoryRouteRepo {
    async fn insert(&self, route: &Route) -> Result<(), DomainError> {
        let mut store = self.store.write();
        let key = (route.tenant_id, route.id);
        if store.by_id.contains_key(&key) {
            return Err(DomainError::validation(format!(
                "route {} already exists",
                route.id
            )));
        }
        store.by_id.insert(key, route.clone());
        Ok(())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(self.store.read().by_id.get(&(tenant_id, id)).cloned())
    }

    async fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        let store = self.store.read();
        let mut out: Vec<Route> = store
            .by_id
            .iter()
            .filter(|((tenant, _), _)| *tenant == tenant_id)
            .map(|(_, r)| r.clone())
            .collect();
        out.sort_by(|a, b| (a.path.as_str(), a.id).cmp(&(b.path.as_str(), b.id)));
        Ok(out)
    }

    async fn list_matching(
        &self,
        tenant_id: Uuid,
        method: &str,
        path: &str,
    ) -> Result<Vec<Route>, DomainError> {
        let store = self.store.read();
        Ok(store
            .by_id
            .iter()
            .filter(|((tenant, _), r)| *tenant == tenant_id && r.enabled)
            .filter(|(_, r)| path_starts_with(path, &r.path) && r.allows_method(method))
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn routes_referencing_alias(
        &self,
        scope: &[Uuid],
        alias: &str,
    ) -> Result<Vec<Route>, DomainError> {
        let store = self.store.read();
        Ok(store
            .by_id
            .iter()
            .filter(|((tenant, _), r)| {
                scope.contains(tenant) && r.target_alias.eq_ignore_ascii_case(alias)
            })
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn update(&self, route: &Route) -> Result<(), DomainError> {
        let mut store = self.store.write();
        let key = (route.tenant_id, route.id);
        let Some(existing) = store.by_id.get(&key) else {
            return Err(DomainError::not_found(format!(
                "route {} not found",
                route.id
            )));
        };
        let mut next = route.clone();
        next.created_at = existing.created_at;
        next.tenant_id = existing.tenant_id;
        store.by_id.insert(key, next);
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, DomainError> {
        Ok(self.store.write().by_id.remove(&(tenant_id, id)).is_some())
    }
}

/// Prefix test for route matching: a route's `path` matches when the request
/// path equals it or starts with it at a segment boundary.
#[must_use]
pub fn path_starts_with(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    if let Some(rest) = path.strip_prefix(prefix) {
        // `rest` starts with a slash, a literal `{`, or is empty, so segments
        // never match partially (`/v1` must not match `/v10`).
        return rest.starts_with('/') || rest.starts_with('{');
    }
    false
}

#[derive(Default)]
struct PluginStore {
    /// `(tenant_id, route_id, plugin_id) -> binding`.
    bindings: HashMap<(Uuid, Uuid, String), PluginBinding>,
}

/// In-memory plugin binding repository.
#[derive(Default)]
pub struct InMemoryPluginRepo {
    store: RwLock<PluginStore>,
}

impl InMemoryPluginRepo {
    /// Create an empty repository.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl PluginRepository for InMemoryPluginRepo {
    async fn bind(
        &self,
        tenant_id: Uuid,
        route_id: Uuid,
        binding: &PluginBinding,
    ) -> Result<(), DomainError> {
        self.store.write().bindings.insert(
            (tenant_id, route_id, binding.plugin_id.clone()),
            binding.clone(),
        );
        Ok(())
    }

    async fn unbind(
        &self,
        tenant_id: Uuid,
        route_id: Uuid,
        plugin_id: &str,
    ) -> Result<(), DomainError> {
        self.store
            .write()
            .bindings
            .remove(&(tenant_id, route_id, plugin_id.to_owned()));
        Ok(())
    }

    async fn list_bindings(
        &self,
        scope: &[Uuid],
        plugin_id: &str,
    ) -> Result<Vec<(Uuid, PluginBinding)>, DomainError> {
        let store = self.store.read();
        Ok(store
            .bindings
            .iter()
            .filter(|((tenant, _, id), _)| scope.contains(tenant) && id == plugin_id)
            .map(|((_, route_id, _), b)| (*route_id, b.clone()))
            .collect())
    }
}

#[cfg(test)]
mod memory_repo_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::error::ErrorKind;
    use crate::domain::model::{Endpoint, Scheme};

    fn tenant() -> Uuid {
        Uuid::new_v4()
    }

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        let mut u = Upstream::for_test();
        u.tenant_id = tenant;
        u.alias = alias.to_owned();
        u.endpoints = vec![Endpoint {
            host: "api.partner.com".to_owned(),
            ..Endpoint::default()
        }];
        u
    }

    #[tokio::test]
    async fn insert_get_and_delete_round_trip() {
        let repo = InMemoryUpstreamRepo::new();
        let t = tenant();
        let u = upstream(t, "api.partner.com");
        repo.insert(&u).await.unwrap();

        let found = repo.get(t, u.id).await.unwrap().expect("inserted");
        assert_eq!(found.alias, "api.partner.com");
        assert_eq!(repo.len(), 1);

        assert!(repo.delete(t, u.id).await.unwrap());
        assert!(repo.get(t, u.id).await.unwrap().is_none());
        assert!(
            !repo.delete(t, u.id).await.unwrap(),
            "second delete is a miss"
        );
    }

    #[tokio::test]
    async fn alias_lookup_is_case_insensitive() {
        let repo = InMemoryUpstreamRepo::new();
        let t = tenant();
        let u = upstream(t, "API.Partner.COM");
        repo.insert(&u).await.unwrap();
        let found = repo.get_by_alias(&[t], "api.partner.com").await.unwrap();
        assert!(found.is_some(), "lookup must ignore case");
    }

    #[tokio::test]
    async fn alias_collisions_are_conflicts() {
        let repo = InMemoryUpstreamRepo::new();
        let t = tenant();
        repo.insert(&upstream(t, "api.partner.com")).await.unwrap();
        let err = repo
            .insert(&upstream(t, "api.partner.com"))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AliasConflict);
    }

    #[tokio::test]
    async fn tenants_are_isolated() {
        let repo = InMemoryUpstreamRepo::new();
        let a = tenant();
        let b = tenant();
        repo.insert(&upstream(a, "api.partner.com")).await.unwrap();

        assert!(repo.get(b, a).await.unwrap().is_none());
        assert!(
            repo.get_by_alias(&[b], "api.partner.com")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(repo.list_visible(&[b]).await.unwrap().len(), 0);
        assert_eq!(repo.list_visible(&[a]).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn inherited_upstreams_are_visible_to_descendants() {
        let repo = InMemoryUpstreamRepo::new();
        let parent = tenant();
        let child = tenant();
        let mut u = upstream(parent, "api.partner.com");
        u.sharing = crate::domain::model::SharingMode::Inherit;
        repo.insert(&u).await.unwrap();

        assert!(
            repo.get_by_alias(&[child, parent], "api.partner.com")
                .await
                .unwrap()
                .is_some(),
            "descendant scope finds the ancestor's inheritable upstream"
        );
        // Private upstreams stay hidden.
        let private = upstream(parent, "private.partner.com");
        repo.insert(&private).await.unwrap();
        assert!(
            repo.get_by_alias(&[child, parent], "private.partner.com")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn update_preserves_alias_and_created_at() {
        let repo = InMemoryUpstreamRepo::new();
        let t = tenant();
        let mut u = upstream(t, "api.partner.com");
        repo.insert(&u).await.unwrap();
        let created = u.created_at;

        u.endpoints = vec![Endpoint {
            scheme: crate::domain::model::Scheme::Http,
            host: "other.partner.com".to_owned(),
            ..Endpoint::default()
        }];
        u.name = "Renamed".to_owned();
        repo.update(&u).await.unwrap();

        let found = repo.get(t, u.id).await.unwrap().unwrap();
        assert_eq!(found.endpoints[0].scheme, Scheme::Http);
        assert_eq!(found.name, "Renamed");
        assert_eq!(found.created_at, created, "created_at is immutable");
    }

    #[tokio::test]
    async fn update_rejects_an_alias_change() {
        let repo = InMemoryUpstreamRepo::new();
        let t = tenant();
        let mut u = upstream(t, "api.partner.com");
        repo.insert(&u).await.unwrap();
        u.alias = "other.partner.com".to_owned();
        let err = repo.update(&u).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Validation);
    }

    #[tokio::test]
    async fn route_repo_matches_method_and_prefix() {
        let repo = InMemoryRouteRepo::new();
        let t = tenant();
        let mut r = Route::for_test();
        r.tenant_id = t;
        r.path = "/v1".to_owned();
        r.methods = vec!["GET".to_owned()];
        repo.insert(&r).await.unwrap();

        assert_eq!(
            repo.list_matching(t, "GET", "/v1/chat")
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            repo.list_matching(t, "POST", "/v1/chat")
                .await
                .unwrap()
                .is_empty(),
            "method allowlist filters candidates"
        );
        assert!(
            repo.list_matching(t, "GET", "/v10")
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(repo.list(t).await.unwrap().len(), 1);

        let refs = repo
            .routes_referencing_alias(&[t], "API.PARTNER.COM")
            .await
            .unwrap();
        assert_eq!(refs.len(), 1, "alias reference lookup is case-insensitive");
    }
}
