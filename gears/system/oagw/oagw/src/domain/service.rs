//! Control plane service: CRUD, alias enforcement and tenant scoping.
//!
//! The control plane owns configuration writes; the data plane reads the
//! same repositories read-only. All operations are strictly scoped to the
//! calling tenant — ancestor resources are invisible through the management
//! API (`DESIGN.md §3.3` Tenant Scoping).

use std::sync::Arc;
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeaderRules, MatchRule, Plugin, PluginKind, PluginsConfig,
    PROTOCOL_GRPC, PROTOCOL_HTTP, RateLimit, Route, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::validation;
use crate::error::{ErrorKind, OagwError};

/// Tenant chain provider, used by the data plane to walk descendant → root.
#[async_trait::async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Return the tenant itself followed by its ancestors, ordered closest
    /// to root.
    async fn chain(&self, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Hierarchy implementation backed by the `tenant-resolver` gear.
pub struct ResolverHierarchy {
    resolver: Arc<dyn tenant_resolver_sdk::TenantResolverClient>,
}

impl ResolverHierarchy {
    /// Wrap a `TenantResolverClient`.
    #[must_use]
    pub fn new(resolver: Arc<dyn tenant_resolver_sdk::TenantResolverClient>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl TenantHierarchy for ResolverHierarchy {
    async fn chain(&self, tenant_id: Uuid) -> Vec<Uuid> {
        let ctx = toolkit_security::SecurityContext::anonymous();
        match self
            .resolver
            .get_ancestors(
                &ctx,
                tenant_resolver_sdk::TenantId(tenant_id),
                &tenant_resolver_sdk::GetAncestorsOptions::default(),
            )
            .await
        {
            Ok(resp) => {
                let mut chain = vec![tenant_id];
                chain.extend(resp.ancestors.iter().map(|t| t.id.0));
                chain
            }
            Err(err) => {
                tracing::warn!(tenant_id = %tenant_id, error = %err, "tenant chain unavailable");
                vec![tenant_id]
            }
        }
    }
}

/// Single-tenant hierarchy used when no resolver is reachable.
#[derive(Debug, Clone, Copy, Default)]
pub struct FlatHierarchy;

#[async_trait::async_trait]
impl TenantHierarchy for FlatHierarchy {
    async fn chain(&self, tenant_id: Uuid) -> Vec<Uuid> {
        vec![tenant_id]
    }
}

/// Timestamp in RFC 3339 with millisecond precision.
#[must_use]
pub fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    crate::domain::time::format_rfc3339(now)
}

type Result<T> = std::result::Result<T, OagwError>;

fn not_found(resource: &str, id: Uuid) -> OagwError {
    OagwError::new(
        ErrorKind::RouteNotFound,
        format!("{resource} {id} not found"),
    )
}

fn conflict(detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::AlreadyExists, detail)
}

/// Control-plane configuration service.
pub struct ControlPlaneService<U, R, P> {
    upstreams: Arc<U>,
    routes: Arc<R>,
    plugins: Arc<P>,
    allow_http: bool,
}

/// Type alias for the in-memory wired control plane.
pub type SharedControlPlaneService =
    ControlPlaneService<crate::infra::storage::MemoryStore, crate::infra::storage::MemoryStore, crate::infra::storage::MemoryStore>;

impl<U, R, P> ControlPlaneService<U, R, P>
where
    U: UpstreamRepository + 'static,
    R: RouteRepository + 'static,
    P: PluginRepository + 'static,
{
    /// Wire a control plane over the given repositories.
    #[must_use]
    pub fn new(upstreams: Arc<U>, routes: Arc<R>, plugins: Arc<P>, allow_http: bool) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            allow_http,
        }
    }

    // -- Upstreams ----------------------------------------------------------

    /// Create an upstream. Returns 409 on alias conflict within the tenant.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_upstream(
        &self,
        tenant_id: Uuid,
        alias: Option<String>,
        enabled: bool,
        server: Vec<Endpoint>,
        protocol: String,
        tags: Vec<String>,
        headers: HeaderRules,
        rate_limit: Option<RateLimit>,
        cors: Option<CorsConfig>,
        auth: Option<AuthConfig>,
        plugins: PluginsConfig,
    ) -> Result<Upstream> {
        let alias = validation::validate_upstream(
            alias.as_deref(),
            &server,
            &protocol,
            rate_limit.as_ref(),
            cors.as_ref(),
            &plugins,
            self.allow_http,
        )?;
        if self
            .upstreams
            .alias_taken(tenant_id, &alias, None)
            .await
            .map_err(storage)?
        {
            return Err(conflict(format!(
                "an upstream with alias '{alias}' already exists for this tenant"
            )));
        }
        let now = now_rfc3339();
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            enabled,
            server: crate::domain::model::ServerConfig { endpoints: server },
            protocol,
            tags,
            headers,
            rate_limit,
            cors,
            auth,
            plugins,
            created_at: now.clone(),
            updated_at: now,
        };
        self.upstreams
            .insert(upstream.clone())
            .await
            .map_err(storage)?;
        Ok(upstream)
    }

    /// Replace an upstream. The alias is immutable.
    #[allow(clippy::too_many_arguments)]
    pub async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        alias: Option<String>,
        enabled: bool,
        server: Vec<Endpoint>,
        protocol: String,
        tags: Vec<String>,
        headers: HeaderRules,
        rate_limit: Option<RateLimit>,
        cors: Option<CorsConfig>,
        auth: Option<AuthConfig>,
        plugins: PluginsConfig,
    ) -> Result<Upstream> {
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .await
            .map_err(storage)?
            .ok_or_else(|| not_found("upstream", id))?;

        validation::enforce_alias_update(&existing.alias, alias.as_deref(), &server)?;
        validation::validate_upstream(
            Some(existing.alias.as_str()),
            &server,
            &protocol,
            rate_limit.as_ref(),
            cors.as_ref(),
            &plugins,
            self.allow_http,
        )?;

        let upstream = Upstream {
            id: existing.id,
            tenant_id: existing.tenant_id,
            alias: existing.alias,
            enabled,
            server: crate::domain::model::ServerConfig { endpoints: server },
            protocol,
            tags,
            headers,
            rate_limit,
            cors,
            auth,
            plugins,
            created_at: existing.created_at,
            updated_at: now_rfc3339(),
        };
        self.upstreams
            .update(upstream.clone())
            .await
            .map_err(storage)?;
        Ok(upstream)
    }

    /// Fetch an upstream owned by the tenant.
    pub async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream> {
        self.upstreams
            .get(tenant_id, id)
            .await
            .map_err(storage)?
            .ok_or_else(|| not_found("upstream", id))
    }

    /// List upstreams owned by the tenant.
    pub async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>> {
        self.upstreams.list(tenant_id).await.map_err(storage)
    }

    /// Delete an upstream and every route attached to it.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<()> {
        if !self.upstreams.get(tenant_id, id).await.map_err(storage)?.is_some() {
            return Err(not_found("upstream", id));
        }
        let attached = self.routes.list(tenant_id, Some(id)).await.map_err(storage)?;
        for route in attached {
            self.routes.delete(tenant_id, route.id).await.map_err(storage)?;
        }
        self.upstreams.delete(tenant_id, id).await.map_err(storage)?;
        Ok(())
    }

    // -- Routes ---------------------------------------------------------

    /// Create a route under an existing upstream.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_route(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        tags: Vec<String>,
        r#match: MatchRule,
        plugins: PluginsConfig,
        rate_limit: Option<RateLimit>,
        cors: Option<CorsConfig>,
        headers: Option<HeaderRules>,
    ) -> Result<Route> {
        let upstream = self
            .upstreams
            .get(tenant_id, upstream_id)
            .await
            .map_err(storage)?
            .ok_or_else(|| {
                OagwError::new(
                    ErrorKind::ValidationError,
                    format!("upstream {upstream_id} not found for this tenant"),
                )
                .with_ext("fields", serde_json::Value::from(["upstream_id"]))
            })?;

        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            tags,
            r#match,
            plugins,
            rate_limit,
            cors,
            headers,
            created_at: now_rfc3339(),
            updated_at: now_rfc3339(),
        };
        validation::validate_route(&route, &upstream)?;
        self.assert_route_unique(&route, &upstream, None).await?;
        self.routes.insert(route.clone()).await.map_err(storage)?;
        Ok(route)
    }

    /// Replace a route. `upstream_id` is immutable.
    #[allow(clippy::too_many_arguments)]
    pub async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        tags: Vec<String>,
        r#match: MatchRule,
        plugins: PluginsConfig,
        rate_limit: Option<RateLimit>,
        cors: Option<CorsConfig>,
        headers: Option<HeaderRules>,
    ) -> Result<Route> {
        let existing = self.routes.get(tenant_id, id).await.map_err(storage)?
            .ok_or_else(|| not_found("route", id))?;
        let upstream = self
            .upstreams
            .get(tenant_id, existing.upstream_id)
            .await
            .map_err(storage)?
            .ok_or_else(|| not_found("upstream", existing.upstream_id))?;

        let route = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            upstream_id: existing.upstream_id,
            tags,
            r#match,
            plugins,
            rate_limit,
            cors,
            headers,
            created_at: existing.created_at,
            updated_at: now_rfc3339(),
        };
        validation::validate_route(&route, &upstream)?;
        self.assert_route_unique(&route, &upstream, Some(existing.id))
            .await?;
        self.routes.update(route.clone()).await.map_err(storage)?;
        Ok(route)
    }

    /// Fetch a route owned by the tenant.
    pub async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route> {
        self.routes.get(tenant_id, id).await.map_err(storage)?
            .ok_or_else(|| not_found("route", id))
    }

    /// List routes owned by the tenant.
    pub async fn list_routes(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> Result<Vec<Route>> {
        self.routes
            .list(tenant_id, upstream_id)
            .await
            .map_err(storage)
    }

    /// Delete a route owned by the tenant.
    pub async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<()> {
        if !self.routes.get(tenant_id, id).await.map_err(storage)?.is_some() {
            return Err(not_found("route", id));
        }
        self.routes.delete(tenant_id, id).await.map_err(storage)?;
        Ok(())
    }

    async fn assert_route_unique(
        &self,
        route: &Route,
        upstream: &Upstream,
        exclude_id: Option<Uuid>,
    ) -> Result<()> {
        let siblings = self
            .routes
            .list(upstream.tenant_id, Some(upstream.id))
            .await
            .map_err(storage)?;
        let clashes = siblings.iter().any(|other| {
            Some(other.id) != exclude_id
                && other.r#match == route.r#match
                && matches!(route.r#match, MatchRule::Http(_))
        });
        if clashes {
            return Err(conflict(
                "a route with the same match rule already exists on this upstream",
            ));
        }
        Ok(())
    }

    // -- Plugins --------------------------------------------------------

    /// Create an immutable custom plugin.
    pub async fn create_plugin(
        &self,
        tenant_id: Uuid,
        plugin_type: &str,
        name: String,
        config_schema: Option<serde_json::Value>,
        source_code: String,
    ) -> Result<Plugin> {
        let kind = classify_plugin_type(plugin_type);
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: kind.gts_fragment().to_owned(),
            name,
            config_schema,
            source_code,
            created_at: now_rfc3339(),
            last_used_at: None,
            gc_eligible_at: None,
        };
        self.plugins.insert(plugin.clone()).await.map_err(storage)?;
        Ok(plugin)
    }

    /// Fetch a plugin owned by the tenant.
    pub async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin> {
        self.plugins.get(tenant_id, id).await.map_err(storage)?
            .ok_or_else(|| not_found("plugin", id))
    }

    /// List plugins owned by the tenant.
    pub async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>> {
        self.plugins.list(tenant_id).await.map_err(storage)
    }

    /// Delete a plugin, refusing when an upstream or route still binds it.
    pub async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<()> {
        if self.plugins.get(tenant_id, id).await.map_err(storage)?.is_none() {
            return Err(not_found("plugin", id));
        }
        let id_str = id.to_string();
        let in_use = {
            let upstreams = self.upstreams.list(tenant_id).await.map_err(storage)?;
            let routes = self.routes.list(tenant_id, None).await.map_err(storage)?;
            let bound = |plugins: &PluginsConfig| {
                plugins.items.iter().any(|b| {
                    b.plugin_ref == id_str
                        || b.plugin_uuid.map(|u| u.to_string()) == Some(id_str.clone())
                })
            };
            let auth_used = upstreams.iter().any(|u| {
                u.auth.as_ref().is_some_and(|a| {
                    a.plugin_id().map(str::to_owned) == Some(id_str.clone())
                        || crate::domain::model::parse_uuid_suffix(a.plugin_id().unwrap_or_default())
                            == Some(id)
                })
            });
            auth_used
                || upstreams.iter().any(|u| bound(&u.plugins))
                || routes.iter().any(|r| bound(&r.plugins))
        };
        if in_use {
            return Err(OagwError::new(
                ErrorKind::PluginInUse,
                format!("plugin {id} is still referenced by an upstream or route"),
            ));
        }
        self.plugins.delete(tenant_id, id).await.map_err(storage)?;
        Ok(())
    }

    // -- Data-plane resolution ------------------------------------------

    /// Resolve an upstream by alias across the tenant chain (closest wins).
    pub async fn resolve_upstream(
        &self,
        hierarchy: &dyn crate::domain::service::TenantHierarchy,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Upstream> {
        let normalized = validation::normalize_alias(alias);
        for scope in hierarchy.chain(tenant_id).await {
            if let Some(found) = self
                .upstreams
                .find_by_alias(scope, &normalized)
                .await
                .map_err(storage)?
            {
                return Ok(found);
            }
        }
        Err(OagwError::new(
            ErrorKind::RouteNotFound,
            format!("no upstream is registered for alias '{alias}'"),
        ))
    }

    /// Match a route for `method` and `path` across the tenant chain.
    pub async fn resolve_route(
        &self,
        hierarchy: &dyn crate::domain::service::TenantHierarchy,
        tenant_id: Uuid,
        upstream_id: Uuid,
        method: &http::Method,
        path_suffix: &str,
    ) -> Result<Route> {
        for scope in hierarchy.chain(tenant_id).await {
            let candidates = self
                .routes
                .list(scope, Some(upstream_id))
                .await
                .map_err(storage)?;
            if let Some(route) = crate::domain::routing::best_match(&candidates, method, path_suffix)
            {
                return Ok(route.clone());
            }
        }
        Err(OagwError::new(
            ErrorKind::RouteNotFound,
            format!("no route matched {method} {path_suffix}"),
        )
        .with_ext("path", path_suffix))
    }
}

/// Map a storage failure onto a gateway error.
fn storage(err: anyhow::Error) -> OagwError {
    tracing::error!(error = %err, "oagw storage failure");
    OagwError::new(
        ErrorKind::LinkUnavailable,
        "configuration storage is unavailable",
    )
}

/// Classify a plugin-type string into a [`PluginKind`].
#[must_use]
pub fn classify_plugin_type(value: &str) -> PluginKind {
    match PluginKind::from_ref(value) {
        Some(kind) => kind,
        None => match value {
            "auth_plugin" | "auth" => PluginKind::Auth,
            "guard_plugin" | "guard" => PluginKind::Guard,
            "transform_plugin" | "transform" => PluginKind::Transform,
            _ => PluginKind::Guard,
        },
    }
}

/// Names of the recognized upstream protocols.
#[must_use]
pub fn known_protocols() -> [&'static str; 2] {
    [PROTOCOL_HTTP, PROTOCOL_GRPC]
}
