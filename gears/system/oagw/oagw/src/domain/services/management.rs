//! Control-plane CRUD service (DESIGN §3.4 "Management API").
//!
//! The service owns every cross-entity rule the REST layer must not have to
//! re-implement: server-generated identifiers, alias derivation, route-match
//! uniqueness, plugin usage accounting and the strict tenant scoping that makes
//! ancestor resources invisible (404) to descendants.

use std::sync::Arc;

use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_millis() as u64)
}

/// Control-plane facade over the three repositories.
///
/// Generic over the repository implementations so the in-memory store shipped
/// with this crate and a relational implementation are interchangeable
/// (`DESIGN` §3.6 schema).
#[derive(Clone)]
pub struct ManagementService<U, R, P> {
    upstreams: Arc<U>,
    routes: Arc<R>,
    plugins: Arc<P>,
    config: Arc<OagwConfig>,
    /// Serialises the check-then-write sequences (route-match uniqueness,
    /// plugin usage accounting) so two concurrent writers cannot both observe
    /// a stale pre-state.
    writes: Arc<tokio::sync::Mutex<()>>,
}

impl<U, R, P> ManagementService<U, R, P>
where
    U: UpstreamRepository,
    R: RouteRepository,
    P: PluginRepository,
{
    /// Creates a service bound to the given repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<U>,
        routes: Arc<R>,
        plugins: Arc<P>,
        config: Arc<OagwConfig>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            config,
            writes: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The gear configuration the service validates against.
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    // === Upstreams ===

    /// Creates an upstream (DESIGN §3.4 POST).
    ///
    /// The identifier and both timestamps are server-generated; the alias is
    /// derived from the endpoint pool unless the pool forces an explicit one.
    ///
    /// # Errors
    ///
    /// Returns a validation error for schema/policy violations and
    /// `AliasConflict` when `(tenant_id, alias)` is already bound.
    pub async fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
    ) -> DomainResult<Upstream> {
        let _guard = self.writes.lock().await;
        upstream.id = Uuid::new_v4();
        upstream.tenant_id = tenant_id;
        upstream.alias =
            crate::domain::validation::validate_upstream_body(&upstream, &self.config)?;
        let now = now_millis();
        upstream.created_at = now;
        upstream.updated_at = now;
        self.upstreams.insert(upstream.clone()).await?;
        Ok(upstream)
    }

    /// Lists the caller's own upstreams (ancestors are invisible).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub async fn list_upstreams(&self, tenant_id: Uuid) -> DomainResult<Vec<Upstream>> {
        self.upstreams.list_by_tenant(tenant_id).await
    }

    /// Fetches one own upstream by id.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when the resource is absent or owned by another
    /// tenant.
    pub async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Upstream> {
        self.upstreams
            .find_by_id(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found("upstream", id.to_string()))
    }

    /// Replaces an upstream (DESIGN §3.4 PUT: full replacement).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource, validation errors for
    /// schema or alias-immutability violations and `AliasConflict` when the new
    /// alias is taken by a sibling.
    pub async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut next: Upstream,
    ) -> DomainResult<Upstream> {
        let _guard = self.writes.lock().await;
        let existing = self.get_upstream(tenant_id, id).await?;
        next.id = existing.id;
        next.tenant_id = existing.tenant_id;
        next.created_at = existing.created_at;
        crate::domain::validation::validate_upstream_update(&existing, &next)?;
        next.alias = crate::domain::validation::validate_upstream_body(&next, &self.config)?;
        next.updated_at = now_millis();
        self.upstreams.replace(next.clone()).await?;
        Ok(next)
    }

    /// Deletes an upstream and cascades to its routes (DESIGN §3.6 FK cascade).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<()> {
        let _guard = self.writes.lock().await;
        // Children go first: a failure after the parent is gone must not leave
        // dangling routes behind (DESIGN §3.6 FK cascade).
        for route in self.routes.list_by_tenant(tenant_id, Some(id)).await? {
            self.routes.delete(tenant_id, route.id).await?;
        }
        if !self.upstreams.delete(tenant_id, id).await? {
            return Err(DomainError::not_found("upstream", id.to_string()));
        }
        Ok(())
    }

    // === Routes ===

    /// Creates a route attached to an own upstream.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for an unknown/foreign upstream, validation errors
    /// for schema violations and `RouteConflict` when an enabled route of the
    /// same upstream already covers the same match key.
    pub async fn create_route(&self, tenant_id: Uuid, mut route: Route) -> DomainResult<Route> {
        let _guard = self.writes.lock().await;
        route.id = Uuid::new_v4();
        route.tenant_id = tenant_id;
        let upstream = self
            .upstream_for_route(tenant_id, route.upstream_id)
            .await?;
        crate::domain::validation::validate_route_body(
            &route,
            Some(&upstream),
            &self.config,
            true,
        )?;
        self.ensure_match_unique(tenant_id, None, &route).await?;
        let now = now_millis();
        route.created_at = now;
        route.updated_at = now;
        self.routes.insert(route.clone()).await?;
        Ok(route)
    }

    /// Lists the caller's own routes, optionally filtered by upstream.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub async fn list_routes(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> DomainResult<Vec<Route>> {
        self.routes.list_by_tenant(tenant_id, upstream_id).await
    }

    /// Fetches one own route by id.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource.
    pub async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Route> {
        self.routes
            .find_by_id(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found("route", id.to_string()))
    }

    /// Replaces a route; `upstream_id` is immutable (DESIGN §3.4 PUT).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource, validation errors for
    /// schema violations and `RouteConflict` when the new match key collides.
    pub async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut next: Route,
    ) -> DomainResult<Route> {
        let _guard = self.writes.lock().await;
        let existing = self.get_route(tenant_id, id).await?;
        next.id = existing.id;
        next.tenant_id = existing.tenant_id;
        next.upstream_id = existing.upstream_id;
        next.priority = existing.priority;
        next.created_at = existing.created_at;
        let upstream = match existing.upstream_id {
            Some(upstream_id) => Some(self.get_upstream(tenant_id, upstream_id).await?),
            None => None,
        };
        crate::domain::validation::validate_route_body(
            &next,
            upstream.as_ref(),
            &self.config,
            false,
        )?;
        self.ensure_match_unique(tenant_id, Some(id), &next).await?;
        next.updated_at = now_millis();
        self.routes.replace(next.clone()).await?;
        Ok(next)
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource.
    pub async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<()> {
        if !self.routes.delete(tenant_id, id).await? {
            return Err(DomainError::not_found("route", id.to_string()));
        }
        Ok(())
    }

    // === Plugins ===

    /// Creates a custom plugin.
    ///
    /// # Errors
    ///
    /// Returns validation errors for schema violations and a conflict error
    /// when `(tenant_id, name)` already exists.
    pub async fn create_plugin(&self, tenant_id: Uuid, mut plugin: Plugin) -> DomainResult<Plugin> {
        plugin.id = Uuid::new_v4();
        plugin.tenant_id = tenant_id;
        crate::domain::validation::validate_plugin_body(&plugin)?;
        plugin.last_used_at = None;
        plugin.gc_eligible_at = None;
        self.plugins.insert(plugin.clone()).await?;
        Ok(plugin)
    }

    /// Lists the caller's own plugins, optionally filtered by type.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub async fn list_plugins(
        &self,
        tenant_id: Uuid,
        plugin_type: Option<&str>,
    ) -> DomainResult<Vec<Plugin>> {
        self.plugins.list_by_tenant(tenant_id, plugin_type).await
    }

    /// Fetches one own plugin by id.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource.
    pub async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Plugin> {
        self.plugins
            .find_by_id(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::not_found("plugin", id.to_string()))
    }

    /// Returns the Starlark source of a custom plugin (DESIGN §3.4).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource and a validation error
    /// for built-in plugins, which have no source.
    pub async fn get_plugin_source(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<String> {
        let plugin = self.get_plugin(tenant_id, id).await?;
        plugin.source_code.clone().ok_or_else(|| {
            DomainError::Validation(format!(
                "plugin {:?} is a built-in plugin and has no Starlark source",
                plugin.name
            ))
        })
    }

    /// Deletes a plugin that is no longer referenced (DESIGN §3.4).
    ///
    /// # Errors
    ///
    /// Returns `NotFound` for a foreign/absent resource and `PluginInUse` when
    /// an upstream or route still binds it.
    pub async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<()> {
        let _guard = self.writes.lock().await;
        self.get_plugin(tenant_id, id).await?;
        if let Some(usage) = self.plugin_usage(tenant_id, id).await? {
            return Err(usage);
        }
        if !self.plugins.delete(tenant_id, id).await? {
            return Err(DomainError::not_found("plugin", id.to_string()));
        }
        Ok(())
    }

    /// Builds the `PluginInUse` error describing where a plugin is referenced,
    /// or `None` when nothing references it.
    async fn plugin_usage(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<Option<DomainError>> {
        let mut upstreams = Vec::new();
        let mut routes = Vec::new();
        let identifier = id.to_string();
        let references = |bindings: &[crate::domain::model::PluginBinding], auth: Option<Uuid>| {
            auth == Some(id)
                || bindings.iter().any(|binding| {
                    binding.plugin_uuid == Some(id)
                        || binding
                            .plugin_ref
                            .rsplit('~')
                            .next()
                            .is_some_and(|instance| instance == identifier)
                })
        };
        for upstream in self.upstreams.list_by_tenant(tenant_id).await? {
            if references(&upstream.plugins.items, upstream.auth.plugin_uuid()) {
                upstreams.push(crate::domain::model::gts::instance(
                    crate::domain::model::gts::UPSTREAM,
                    &upstream.id,
                ));
            }
        }
        for route in self.routes.list_by_tenant(tenant_id, None).await? {
            if references(&route.plugins.items, None) {
                routes.push(crate::domain::model::gts::instance(
                    crate::domain::model::gts::ROUTE,
                    &route.id,
                ));
            }
        }
        Ok(
            (!upstreams.is_empty() || !routes.is_empty()).then_some(DomainError::PluginInUse {
                plugin_id: id.to_string(),
                upstreams,
                routes,
            }),
        )
    }

    /// Marks a plugin as used by the data plane (GC bookkeeping).
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub async fn touch_plugin(&self, tenant_id: Uuid, id: Uuid) -> DomainResult<()> {
        self.plugins.touch(tenant_id, id, now_millis()).await
    }

    /// Marks an unlinked custom plugin as garbage-collectable.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be written.
    pub async fn mark_plugin_gc_eligible(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        eligible_at: u64,
    ) -> DomainResult<()> {
        self.plugins
            .mark_gc_eligible(tenant_id, id, eligible_at)
            .await
    }

    async fn upstream_for_route(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> DomainResult<Upstream> {
        let Some(upstream_id) = upstream_id else {
            return Err(DomainError::Validation(
                "route.upstream_id is required: routes attach to exactly one upstream".to_owned(),
            ));
        };
        // `find_by_id` is tenant-scoped, so ancestor upstreams are 404 here —
        // they are not directly addressable (DESIGN §3.4 Tenant Scoping).
        self.upstreams
            .find_by_id(tenant_id, upstream_id)
            .await?
            .ok_or_else(|| DomainError::not_found("upstream", upstream_id.to_string()))
    }

    /// Enforces the route-match determinism invariant (DESIGN §3.6): no two
    /// enabled routes under the same upstream may share a match key.
    async fn ensure_match_unique(
        &self,
        tenant_id: Uuid,
        replacing: Option<Uuid>,
        candidate: &Route,
    ) -> DomainResult<()> {
        if !candidate.enabled {
            return Ok(());
        }
        for existing in self.routes.list_by_tenant(tenant_id, None).await? {
            if Some(existing.id) == replacing || existing.upstream_id != candidate.upstream_id {
                continue;
            }
            if !existing.enabled {
                continue;
            }
            if crate::domain::validation::same_match_key(&existing.r#match, &candidate.r#match)
                && existing.priority == candidate.priority
            {
                return Err(DomainError::RouteConflict(format!(
                    "an enabled route already matches {} for this upstream",
                    crate::domain::validation::describe_match(&candidate.r#match)
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Protocol, Scheme,
        ServerConfig,
    };
    use crate::infra::storage::InMemoryStore;
    use std::sync::Arc;

    fn config() -> Arc<OagwConfig> {
        // Matches `config/e2e-local.yaml`: the local test upstreams are
        // plaintext, so the plaintext gate is disabled for these fixtures.
        Arc::new(OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        })
    }

    fn service() -> ManagementService<InMemoryStore, InMemoryStore, InMemoryStore> {
        let store = Arc::new(InMemoryStore::new());
        ManagementService::new(store.clone(), store.clone(), store, config())
    }

    fn upstream(alias: &str, host: &str, port: u16) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            alias: alias.to_owned(),
            protocol: Protocol::Http,
            enabled: true,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: host.to_owned(),
                    port,
                }],
            },
            auth: crate::domain::model::AuthConfig::default(),
            headers: crate::domain::model::HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn route(upstream_id: Option<Uuid>, path: &str) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id,
            r#match: MatchConfig::Http(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            priority: 0,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[tokio::test]
    async fn create_upstream_derives_the_alias_and_stamps_the_identity() {
        let service = service();
        let tenant = Uuid::new_v4();
        let created = service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        assert_eq!(created.alias, "api.openai.com:8080");
        assert_ne!(created.id, Uuid::nil());
        assert!(created.created_at > 0);
        assert_eq!(created.tenant_id, tenant);
        // The GTS instance identifier uses the server-generated UUID.
        assert!(created.gts_id().ends_with(created.id.to_string().as_str()));
    }

    #[tokio::test]
    async fn create_upstream_reports_alias_conflicts() {
        let service = service();
        let tenant = Uuid::new_v4();
        service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        let conflict = service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect_err("conflict");
        assert!(matches!(conflict, DomainError::AliasConflict { .. }));
        assert_eq!(conflict.status_code(), 409);
    }

    #[tokio::test]
    async fn ancestor_upstreams_are_invisible_to_descendants() {
        let service = service();
        let ancestor = Uuid::new_v4();
        let created = service
            .create_upstream(ancestor, upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        let descendant = Uuid::new_v4();
        assert!(service.get_upstream(descendant, created.id).await.is_err());
        assert_eq!(
            service
                .list_upstreams(descendant)
                .await
                .expect("list")
                .len(),
            0
        );
        assert!(
            service
                .delete_upstream(descendant, created.id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn routes_require_an_addressable_upstream() {
        let service = service();
        let tenant = Uuid::new_v4();
        let ancestor_upstream = service
            .create_upstream(Uuid::new_v4(), upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        let missing = service
            .create_route(tenant, route(Some(ancestor_upstream.id), "/v1"))
            .await
            .expect_err("foreign upstream is 404");
        assert_eq!(missing.status_code(), 404);
        let none = service
            .create_route(tenant, route(None, "/v1"))
            .await
            .expect_err("upstream_id is required");
        assert_eq!(none.status_code(), 400);
    }

    #[tokio::test]
    async fn duplicate_match_keys_are_rejected() {
        let service = service();
        let tenant = Uuid::new_v4();
        let created = service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        service
            .create_route(tenant, route(Some(created.id), "/v1"))
            .await
            .expect("first route");
        let duplicate = service
            .create_route(tenant, route(Some(created.id), "/v1"))
            .await
            .expect_err("conflict");
        assert!(matches!(duplicate, DomainError::RouteConflict(_)));
        assert_eq!(duplicate.status_code(), 409);
        // A different path is fine.
        service
            .create_route(tenant, route(Some(created.id), "/v2"))
            .await
            .expect("second route");
    }

    #[tokio::test]
    async fn replace_route_keeps_upstream_id_immutable() {
        let service = service();
        let tenant = Uuid::new_v4();
        let created = service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        let stored = service
            .create_route(tenant, route(Some(created.id), "/v1"))
            .await
            .expect("route");
        let mut next = route(Some(created.id), "/v2");
        next.id = stored.id;
        next.upstream_id = Some(Uuid::new_v4());
        let updated = service
            .replace_route(tenant, stored.id, next)
            .await
            .expect("replaced");
        assert_eq!(updated.upstream_id, Some(created.id));
        assert_eq!(updated.match_path(), "/v2");
    }

    #[tokio::test]
    async fn delete_upstream_cascades_to_routes() {
        let service = service();
        let tenant = Uuid::new_v4();
        let created = service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect("created");
        let stored = service
            .create_route(tenant, route(Some(created.id), "/v1"))
            .await
            .expect("route");
        service
            .delete_upstream(tenant, created.id)
            .await
            .expect("deleted");
        assert!(service.get_upstream(tenant, created.id).await.is_err());
        assert!(service.get_route(tenant, stored.id).await.is_err());
    }

    #[tokio::test]
    async fn referenced_plugins_cannot_be_deleted() {
        let service = service();
        let tenant = Uuid::new_v4();
        let plugin = service
            .create_plugin(
                tenant,
                Plugin {
                    id: Uuid::new_v4(),
                    tenant_id: tenant,
                    plugin_type: "transform".to_owned(),
                    name: "annotate".to_owned(),
                    description: None,
                    config_schema: None,
                    source_code: Some("def transform(request):\n    return request\n".to_owned()),
                    last_used_at: None,
                    gc_eligible_at: None,
                },
            )
            .await
            .expect("created");
        let created = service
            .create_upstream(tenant, upstream("", "api.openai.com", 8080))
            .await
            .expect("upstream");
        let mut bound = route(Some(created.id), "/v1");
        bound.plugins = crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::Sharing::Private,
            items: vec![crate::domain::model::PluginBinding::from_ref(
                plugin.id.to_string(),
            )],
        };
        service.create_route(tenant, bound).await.expect("route");

        let in_use = service
            .delete_plugin(tenant, plugin.id)
            .await
            .expect_err("referenced");
        assert!(matches!(in_use, DomainError::PluginInUse { .. }));
        assert_eq!(in_use.status_code(), 409);

        service
            .delete_route(tenant, {
                let routes = service.list_routes(tenant, None).await.expect("routes");
                routes[0].id
            })
            .await
            .expect("route deleted");
        service
            .delete_plugin(tenant, plugin.id)
            .await
            .expect("plugin deleted");
    }

    #[tokio::test]
    async fn plugin_source_round_trips_for_custom_plugins() {
        let service = service();
        let tenant = Uuid::new_v4();
        let plugin = service
            .create_plugin(
                tenant,
                Plugin {
                    id: Uuid::new_v4(),
                    tenant_id: tenant,
                    plugin_type: "guard".to_owned(),
                    name: "annotate".to_owned(),
                    description: None,
                    config_schema: None,
                    source_code: Some("def guard(request):\n    return request\n".to_owned()),
                    last_used_at: None,
                    gc_eligible_at: None,
                },
            )
            .await
            .expect("created");
        let source = service
            .get_plugin_source(tenant, plugin.id)
            .await
            .expect("source");
        assert!(source.contains("def guard"));
        // Source is tenant-scoped.
        assert!(
            service
                .get_plugin_source(Uuid::new_v4(), plugin.id)
                .await
                .is_err()
        );
        // Unknown ids are 404.
        let missing = service
            .get_plugin_source(tenant, Uuid::new_v4())
            .await
            .expect_err("missing");
        assert_eq!(missing.status_code(), 404);
    }

    #[tokio::test]
    async fn plugins_without_source_are_rejected() {
        let service = service();
        let tenant = Uuid::new_v4();
        let error = service
            .create_plugin(
                tenant,
                Plugin {
                    id: Uuid::new_v4(),
                    tenant_id: tenant,
                    plugin_type: "guard".to_owned(),
                    name: "no-source".to_owned(),
                    description: None,
                    config_schema: None,
                    source_code: None,
                    last_used_at: None,
                    gc_eligible_at: None,
                },
            )
            .await
            .expect_err("validation");
        assert_eq!(error.status_code(), 400);
    }

    #[tokio::test]
    async fn plugin_names_are_unique_per_tenant() {
        let service = service();
        let tenant = Uuid::new_v4();
        let base = Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            plugin_type: "guard".to_owned(),
            name: "annotate".to_owned(),
            description: None,
            config_schema: None,
            source_code: Some("def guard(request):\n    return request\n".to_owned()),
            last_used_at: None,
            gc_eligible_at: None,
        };
        service
            .create_plugin(tenant, base.clone())
            .await
            .expect("created");
        let duplicate = service
            .create_plugin(tenant, base)
            .await
            .expect_err("conflict");
        assert_eq!(duplicate.status_code(), 409);
    }
}
