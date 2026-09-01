//! In-memory control-plane storage.
//!
//! Implements the `domain::repo` traits over shared maps. This is the storage
//! engine for the current crate manifest (no `toolkit-db`/SeaORM dependency);
//! the trait boundary keeps a relational implementation a drop-in replacement.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// In-memory store for the three configuration entities.
#[derive(Clone, Default)]
pub struct InMemoryStore {
    upstreams: Arc<RwLock<BTreeMap<Uuid, Upstream>>>,
    routes: Arc<RwLock<BTreeMap<Uuid, Route>>>,
    plugins: Arc<RwLock<BTreeMap<Uuid, Plugin>>>,
}

impl InMemoryStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl UpstreamRepository for InMemoryStore {
    async fn insert(&self, upstream: Upstream) -> DomainResult<()> {
        let mut guard = self.upstreams.write();
        if guard.values().any(|existing| {
            existing.tenant_id == upstream.tenant_id && existing.alias == upstream.alias
        }) {
            return Err(DomainError::AliasConflict {
                alias: upstream.alias.clone(),
            });
        }
        guard.insert(upstream.id, upstream);
        Ok(())
    }

    async fn replace(&self, upstream: Upstream) -> DomainResult<()> {
        let mut guard = self.upstreams.write();
        if !guard
            .get(&upstream.id)
            .is_some_and(|stored| stored.tenant_id == upstream.tenant_id)
        {
            return Err(DomainError::not_found("upstream", upstream.id.to_string()));
        }
        if guard.values().any(|existing| {
            existing.id != upstream.id
                && existing.tenant_id == upstream.tenant_id
                && existing.alias == upstream.alias
        }) {
            return Err(DomainError::AliasConflict {
                alias: upstream.alias.clone(),
            });
        }
        guard.insert(upstream.id, upstream);
        Ok(())
    }

    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .get(&id)
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned())
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> DomainResult<Option<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .values()
            .find(|upstream| upstream.tenant_id == tenant_id && upstream.alias == alias)
            .cloned())
    }

    async fn list_by_tenant(&self, tenant_id: Uuid) -> DomainResult<Vec<Upstream>> {
        Ok(self
            .upstreams
            .read()
            .values()
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> DomainResult<Vec<Upstream>> {
        Ok(self.upstreams.read().values().cloned().collect())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let mut guard = self.upstreams.write();
        let removable = guard
            .get(&id)
            .is_some_and(|upstream| upstream.tenant_id == tenant_id);
        if removable {
            guard.remove(&id);
        }
        Ok(removable)
    }
}

#[async_trait]
impl RouteRepository for InMemoryStore {
    async fn insert(&self, route: Route) -> DomainResult<()> {
        let mut guard = self.routes.write();
        if guard.contains_key(&route.id) {
            return Err(DomainError::ConcurrentModification(format!(
                "route {} already exists",
                route.id
            )));
        }
        guard.insert(route.id, route);
        Ok(())
    }

    async fn replace(&self, route: Route) -> DomainResult<()> {
        let mut guard = self.routes.write();
        if !guard
            .get(&route.id)
            .is_some_and(|stored| stored.tenant_id == route.tenant_id)
        {
            return Err(DomainError::not_found("route", route.id.to_string()));
        }
        guard.insert(route.id, route);
        Ok(())
    }

    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Route>> {
        Ok(self
            .routes
            .read()
            .get(&id)
            .filter(|route| route.tenant_id == tenant_id)
            .cloned())
    }

    async fn list_by_tenant(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> DomainResult<Vec<Route>> {
        Ok(self
            .routes
            .read()
            .values()
            .filter(|route| {
                route.tenant_id == tenant_id
                    && upstream_id.is_none_or(|upstream| route.upstream_id == Some(upstream))
            })
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> DomainResult<Vec<Route>> {
        Ok(self.routes.read().values().cloned().collect())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let mut guard = self.routes.write();
        let removable = guard
            .get(&id)
            .is_some_and(|route| route.tenant_id == tenant_id);
        if removable {
            guard.remove(&id);
        }
        Ok(removable)
    }
}

#[async_trait]
impl PluginRepository for InMemoryStore {
    async fn insert(&self, plugin: Plugin) -> DomainResult<()> {
        let mut guard = self.plugins.write();
        if guard
            .values()
            .any(|existing| existing.tenant_id == plugin.tenant_id && existing.name == plugin.name)
        {
            return Err(DomainError::ConcurrentModification(format!(
                "a plugin named {:?} already exists in this tenant",
                plugin.name
            )));
        }
        guard.insert(plugin.id, plugin);
        Ok(())
    }

    async fn find_by_id(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<Plugin>> {
        Ok(self
            .plugins
            .read()
            .get(&id)
            .filter(|plugin| plugin.tenant_id == tenant_id)
            .cloned())
    }

    async fn find_by_name(&self, tenant_id: Uuid, name: &str) -> DomainResult<Option<Plugin>> {
        Ok(self
            .plugins
            .read()
            .values()
            .find(|plugin| plugin.tenant_id == tenant_id && plugin.name == name)
            .cloned())
    }

    async fn list_by_tenant(
        &self,
        tenant_id: Uuid,
        plugin_type: Option<&str>,
    ) -> DomainResult<Vec<Plugin>> {
        Ok(self
            .plugins
            .read()
            .values()
            .filter(|plugin| {
                plugin.tenant_id == tenant_id
                    && plugin_type.is_none_or(|kind| plugin.plugin_type == kind)
            })
            .cloned()
            .collect())
    }

    async fn touch(&self, tenant_id: Uuid, id: Uuid, now_millis: u64) -> DomainResult<()> {
        if let Some(plugin) = self
            .plugins
            .write()
            .get_mut(&id)
            .filter(|plugin| plugin.tenant_id == tenant_id)
        {
            plugin.last_used_at = Some(now_millis);
            plugin.gc_eligible_at = None;
        }
        Ok(())
    }

    async fn mark_gc_eligible(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        eligible_at_millis: u64,
    ) -> DomainResult<()> {
        if let Some(plugin) = self
            .plugins
            .write()
            .get_mut(&id)
            .filter(|plugin| plugin.tenant_id == tenant_id)
        {
            plugin.gc_eligible_at = Some(eligible_at_millis);
        }
        Ok(())
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<bool> {
        let mut guard = self.plugins.write();
        let removable = guard
            .get(&id)
            .is_some_and(|plugin| plugin.tenant_id == tenant_id);
        if removable {
            guard.remove(&id);
        }
        Ok(removable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        AuthConfig, Endpoint, HeadersConfig, HttpMatch, HttpMethod, MatchConfig, Protocol,
        ServerConfig, Upstream,
    };
    use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            alias: alias.to_owned(),
            protocol: Protocol::Http,
            enabled: true,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: crate::domain::model::Scheme::Http,
                    host: "mock.internal".to_owned(),
                    port: 8080,
                }],
            },
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn route(tenant: Uuid, upstream_id: Uuid) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: Some(upstream_id),
            r#match: MatchConfig::Http(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            priority: 0,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 1,
            updated_at: 1,
        }
    }

    #[tokio::test]
    async fn enforces_alias_uniqueness_per_tenant() {
        let store = InMemoryStore::new();
        let tenant = Uuid::new_v4();
        UpstreamRepository::insert(&store, upstream(tenant, "api.openai.com"))
            .await
            .expect("inserted");
        let conflict = UpstreamRepository::insert(&store, upstream(tenant, "api.openai.com")).await;
        assert!(matches!(conflict, Err(DomainError::AliasConflict { .. })));
        let other_tenant = Uuid::new_v4();
        UpstreamRepository::insert(&store, upstream(other_tenant, "api.openai.com"))
            .await
            .expect("same alias in another tenant is allowed");
    }

    #[tokio::test]
    async fn tenant_scoping_hides_resources() {
        let store = InMemoryStore::new();
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let created = upstream(owner, "api.openai.com");
        UpstreamRepository::insert(&store, created.clone())
            .await
            .expect("inserted");
        assert!(
            UpstreamRepository::find_by_id(&store, stranger, created.id)
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            UpstreamRepository::find_by_id(&store, owner, created.id)
                .await
                .expect("read")
                .is_some()
        );
        assert_eq!(
            UpstreamRepository::list_by_tenant(&store, stranger)
                .await
                .expect("read")
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn route_listing_filters_by_upstream() {
        let store = InMemoryStore::new();
        let tenant = Uuid::new_v4();
        let target = upstream(tenant, "api.openai.com");
        UpstreamRepository::insert(&store, target.clone())
            .await
            .expect("inserted");
        let first = route(tenant, target.id);
        RouteRepository::insert(&store, first.clone())
            .await
            .expect("inserted");
        let second = route(tenant, Uuid::new_v4());
        RouteRepository::insert(&store, second.clone())
            .await
            .expect("inserted");
        let for_target = RouteRepository::list_by_tenant(&store, tenant, Some(target.id))
            .await
            .expect("read");
        assert_eq!(for_target.len(), 1);
        assert_eq!(for_target[0].id, first.id);
        assert_eq!(
            RouteRepository::list_by_tenant(&store, tenant, None)
                .await
                .expect("read")
                .len(),
            2
        );
        assert!(
            RouteRepository::delete(&store, tenant, first.id)
                .await
                .expect("delete")
        );
        assert!(
            !RouteRepository::delete(&store, tenant, first.id)
                .await
                .expect("delete")
        );
    }

    #[tokio::test]
    async fn plugin_repository_scopes_by_tenant_and_name() {
        let store = InMemoryStore::new();
        let tenant = Uuid::new_v4();
        let other = Uuid::new_v4();
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            plugin_type: crate::domain::model::gts::GUARD_PLUGIN.to_owned()
                + "cf.core.oagw.required_headers.v1",
            name: "require-auth".to_owned(),
            description: None,
            config_schema: None,
            source_code: None,
            last_used_at: None,
            gc_eligible_at: None,
        };
        PluginRepository::insert(&store, plugin.clone())
            .await
            .expect("inserted");
        let duplicate = Plugin {
            id: Uuid::new_v4(),
            ..plugin.clone()
        };
        assert!(PluginRepository::insert(&store, duplicate).await.is_err());

        let found = PluginRepository::find_by_name(&store, tenant, "require-auth")
            .await
            .expect("read")
            .expect("found");
        assert_eq!(found.id, plugin.id);
        assert!(
            PluginRepository::find_by_name(&store, other, "require-auth")
                .await
                .expect("read")
                .is_none()
        );

        PluginRepository::touch(&store, tenant, plugin.id, 42)
            .await
            .expect("touched");
        let touched = PluginRepository::find_by_id(&store, tenant, plugin.id)
            .await
            .expect("read")
            .expect("found");
        assert_eq!(touched.last_used_at, Some(42));
        PluginRepository::mark_gc_eligible(&store, tenant, plugin.id, 99)
            .await
            .expect("marked");
        let marked = PluginRepository::find_by_id(&store, tenant, plugin.id)
            .await
            .expect("read")
            .expect("found");
        assert_eq!(marked.gc_eligible_at, Some(99));
        assert!(
            PluginRepository::delete(&store, tenant, plugin.id)
                .await
                .expect("delete")
        );
        assert!(
            !PluginRepository::delete(&store, tenant, plugin.id)
                .await
                .expect("delete")
        );
    }
}
