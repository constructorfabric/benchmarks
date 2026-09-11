//! Control Plane: configuration ownership and CRUD.
//!
//! Every operation here is strictly scoped to the calling tenant
//! (`cpt-cf-oagw-nfr-multi-tenancy`); an ancestor's resources are invisible
//! (404) through this surface and only become reachable at proxy time via the
//! tenant-chain walk in [`super::resolve`].

use std::sync::Arc;

use serde_json::{Map, Value};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::OagwError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, Plugin, PluginKind, PluginPhase,
    PluginsConfig, RateLimitConfig, Route, RouteSpec, ServerConfig, Upstream, UpstreamSpec,
};
use crate::domain::ports::{PluginCatalog, TenantDirectory};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::timeutil;
use crate::domain::validate;

/// Everything a create/replace body may carry for an upstream.
#[derive(Debug, Clone, Default)]
pub struct UpstreamInput {
    /// Enable flag; `None` means "use the schema default" (`true`).
    pub enabled: Option<bool>,
    /// Explicit alias; only legal for non-derivable endpoint pools.
    pub alias: Option<String>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool (required).
    pub server: Option<ServerConfig>,
    /// Protocol identifier (required).
    pub protocol: Option<String>,
    /// Auth plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Guard/transform chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
}

/// Everything a create/replace body may carry for a route.
#[derive(Debug, Clone, Default)]
pub struct RouteInput {
    /// Enable flag; `None` means `true`.
    pub enabled: Option<bool>,
    /// Match priority; `None` means `0`.
    pub priority: Option<i32>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Parent upstream — required on create, ignored on replace.
    pub upstream_id: Option<Uuid>,
    /// Inbound match rules (required).
    pub match_config: Option<MatchConfig>,
    /// Guard/transform chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy override.
    pub cors: Option<CorsConfig>,
}

/// Everything a create body may carry for a custom plugin.
#[derive(Debug, Clone, Default)]
pub struct PluginInput {
    /// Plugin kind, either explicit or recovered from `plugin_type`.
    pub kind: Option<PluginKind>,
    /// Tenant-unique name (required).
    pub name: Option<String>,
    /// Human-readable description.
    pub description: Option<String>,
    /// Declared transform phases.
    pub phases: Vec<PluginPhase>,
    /// JSON Schema for the plugin configuration.
    pub config_schema: Option<Value>,
    /// Starlark source (required).
    pub source_code: Option<String>,
}

/// Control Plane service.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    catalog: Arc<dyn PluginCatalog>,
    tenants: Arc<dyn TenantDirectory>,
}

impl ControlPlaneService {
    /// Wire the service to its ports.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        catalog: Arc<dyn PluginCatalog>,
        tenants: Arc<dyn TenantDirectory>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            catalog,
            tenants,
        }
    }

    /// Upstream repository, for the Data Plane's resolution walk.
    #[must_use]
    pub fn upstream_repo(&self) -> &Arc<dyn UpstreamRepository> {
        &self.upstreams
    }

    /// Route repository, for the Data Plane's resolution walk.
    #[must_use]
    pub fn route_repo(&self) -> &Arc<dyn RouteRepository> {
        &self.routes
    }

    /// Plugin repository.
    #[must_use]
    pub fn plugin_repo(&self) -> &Arc<dyn PluginRepository> {
        &self.plugins
    }

    /// Tenant directory.
    #[must_use]
    pub fn tenant_directory(&self) -> &Arc<dyn TenantDirectory> {
        &self.tenants
    }

    /// Reject the nil tenant outright: it carries no isolation boundary, so a
    /// request bearing it must never reach configuration or an upstream.
    fn tenant_of(ctx: &SecurityContext) -> Result<Uuid, OagwError> {
        let tenant_id = ctx.subject_tenant_id();
        if tenant_id.is_nil() {
            return Err(OagwError::forbidden(
                "the request carries no tenant; OAGW resources are tenant-scoped",
            ));
        }
        Ok(tenant_id)
    }

    // -- upstreams ----------------------------------------------------------

    /// Validate `input` and build the normalized spec for a create.
    async fn build_upstream_spec(
        &self,
        tenant_id: Uuid,
        input: UpstreamInput,
        existing_alias: Option<&str>,
    ) -> Result<UpstreamSpec, OagwError> {
        let server = input
            .server
            .ok_or_else(|| OagwError::validation("server is required"))?;
        let protocol = input
            .protocol
            .ok_or_else(|| OagwError::validation("protocol is required"))?;

        validate::validate_server(&server)?;
        validate::validate_protocol(&protocol, &server)?;
        validate::validate_tags(&input.tags)?;

        // Normalize hosts so alias derivation and endpoint selection agree.
        let server = ServerConfig {
            endpoints: server
                .endpoints
                .into_iter()
                .map(|mut e| {
                    e.host = alias::normalize_host(&e.host);
                    e.scheme = e.scheme.trim().to_ascii_lowercase();
                    e
                })
                .collect(),
        };

        let resolved_alias = match existing_alias {
            None => alias::resolve_alias_for_create(&server.endpoints, input.alias.as_deref())?,
            Some(existing) => {
                alias::enforce_alias_update(existing, &server.endpoints, input.alias.as_deref())?
            }
        };

        if let Some(auth) = &input.auth
            && let Some(plugin_ref) = &auth.plugin_type
            && let validate::PluginRefKind::Custom(uuid) =
                validate::validate_auth_ref(plugin_ref, self.catalog.as_ref())?
            && self.plugins.get(tenant_id, uuid).await.is_none()
        {
            return Err(OagwError::validation(format!(
                "auth plugin '{plugin_ref}' does not exist for this tenant"
            )));
        }

        if let Some(plugins) = &input.plugins {
            let custom = validate::validate_plugins(plugins, self.catalog.as_ref())?;
            for uuid in custom {
                if self.plugins.get(tenant_id, uuid).await.is_none() {
                    return Err(OagwError::validation(format!(
                        "plugin '{uuid}' does not exist for this tenant"
                    )));
                }
            }
        }

        if let Some(rate_limit) = &input.rate_limit {
            validate::validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &input.cors {
            validate::validate_cors(cors)?;
        }

        Ok(UpstreamSpec {
            enabled: input.enabled.unwrap_or(true),
            alias: resolved_alias,
            tags: normalize_tags(input.tags),
            server,
            protocol,
            auth: input.auth,
            headers: input.headers,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
        })
    }

    /// Create an upstream.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `403` for a tenantless caller, `409` when
    /// `(tenant_id, alias)` is taken.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamInput,
    ) -> Result<Upstream, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        let spec = self.build_upstream_spec(tenant_id, input, None).await?;
        let now = timeutil::now_rfc3339();
        self.upstreams
            .insert(Upstream {
                id: Uuid::new_v4(),
                tenant_id,
                created_at: now.clone(),
                updated_at: now,
                spec,
            })
            .await
    }

    /// Replace an upstream wholesale; omitted optional blocks are cleared.
    ///
    /// # Errors
    ///
    /// `400` on validation failure (including an alias-moving endpoint
    /// change), `403` for a tenantless caller, `404` when the upstream is not
    /// visible to the caller.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .await
            .ok_or_else(|| OagwError::not_found(format!("upstream '{id}' not found")))?;
        let spec = self
            .build_upstream_spec(tenant_id, input, Some(&existing.spec.alias))
            .await?;
        self.upstreams
            .replace(Upstream {
                id: existing.id,
                tenant_id,
                created_at: existing.created_at,
                updated_at: timeutil::now_rfc3339(),
                spec,
            })
            .await
    }

    /// Fetch an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller, `404` when not visible.
    pub async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        self.upstreams
            .get(tenant_id, id)
            .await
            .ok_or_else(|| OagwError::not_found(format!("upstream '{id}' not found")))
    }

    /// List the calling tenant's upstreams.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller.
    pub async fn list_upstreams(&self, ctx: &SecurityContext) -> Result<Vec<Upstream>, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        Ok(self.upstreams.list(tenant_id).await)
    }

    /// Delete an upstream and cascade to its routes.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller, `404` when not visible.
    pub async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        if !self.upstreams.delete(tenant_id, id).await {
            return Err(OagwError::not_found(format!("upstream '{id}' not found")));
        }
        self.routes.delete_by_upstream(id).await;
        Ok(())
    }

    // -- routes -------------------------------------------------------------

    async fn build_route_spec(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
        input: &RouteInput,
    ) -> Result<RouteSpec, OagwError> {
        let mut match_config = input
            .match_config
            .clone()
            .ok_or_else(|| OagwError::validation("match is required"))?;
        if let Some(http) = &mut match_config.http {
            http.path = validate::normalize_route_path(&http.path);
            http.methods = http
                .methods
                .iter()
                .map(|m| m.trim().to_ascii_uppercase())
                .collect();
        }
        validate::validate_match(&match_config, &upstream.spec.protocol)?;
        validate::validate_tags(&input.tags)?;

        if let Some(plugins) = &input.plugins {
            let custom = validate::validate_plugins(plugins, self.catalog.as_ref())?;
            for uuid in custom {
                if self.plugins.get(tenant_id, uuid).await.is_none() {
                    return Err(OagwError::validation(format!(
                        "plugin '{uuid}' does not exist for this tenant"
                    )));
                }
            }
        }
        if let Some(rate_limit) = &input.rate_limit {
            validate::validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &input.cors {
            validate::validate_cors(cors)?;
        }

        Ok(RouteSpec {
            enabled: input.enabled.unwrap_or(true),
            priority: input.priority.unwrap_or(0),
            tags: normalize_tags(input.tags.clone()),
            match_config,
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
        })
    }

    /// Create a route under one of the calling tenant's upstreams.
    ///
    /// # Errors
    ///
    /// `400` when `upstream_id` is missing or does not belong to the caller
    /// (an ancestor's upstream is not directly addressable), `403` for a
    /// tenantless caller, `409` on a match-rule collision.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        input: RouteInput,
    ) -> Result<Route, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        let upstream_id = input
            .upstream_id
            .ok_or_else(|| OagwError::validation("upstream_id is required"))?;
        let upstream = self
            .upstreams
            .get(tenant_id, upstream_id)
            .await
            .ok_or_else(|| {
                OagwError::validation(format!(
                    "upstream '{upstream_id}' does not exist for this tenant"
                ))
            })?;
        let spec = self.build_route_spec(tenant_id, &upstream, &input).await?;
        let now = timeutil::now_rfc3339();
        self.routes
            .insert(Route {
                id: Uuid::new_v4(),
                tenant_id,
                upstream_id,
                created_at: now.clone(),
                updated_at: now,
                spec,
            })
            .await
    }

    /// Replace a route wholesale. `upstream_id` is immutable and any value in
    /// the body is ignored.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `403` for a tenantless caller, `404` when
    /// not visible, `409` on a match-rule collision.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        let existing = self
            .routes
            .get(tenant_id, id)
            .await
            .ok_or_else(|| OagwError::not_found(format!("route '{id}' not found")))?;
        if let Some(requested) = input.upstream_id
            && requested != existing.upstream_id
        {
            return Err(OagwError::validation(
                "upstream_id is immutable; delete and re-create the route instead",
            ));
        }
        let upstream = self
            .upstreams
            .get(tenant_id, existing.upstream_id)
            .await
            .ok_or_else(|| {
                OagwError::validation("the route's upstream no longer exists for this tenant")
            })?;
        let spec = self.build_route_spec(tenant_id, &upstream, &input).await?;
        self.routes
            .replace(Route {
                id: existing.id,
                tenant_id,
                upstream_id: existing.upstream_id,
                created_at: existing.created_at,
                updated_at: timeutil::now_rfc3339(),
                spec,
            })
            .await
    }

    /// Fetch a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller, `404` when not visible.
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<Route, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        self.routes
            .get(tenant_id, id)
            .await
            .ok_or_else(|| OagwError::not_found(format!("route '{id}' not found")))
    }

    /// List the calling tenant's routes.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller.
    pub async fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<Route>, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        Ok(self.routes.list(tenant_id).await)
    }

    /// Delete a route.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller, `404` when not visible.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        if self.routes.delete(tenant_id, id).await {
            Ok(())
        } else {
            Err(OagwError::not_found(format!("route '{id}' not found")))
        }
    }

    // -- plugins ------------------------------------------------------------

    /// Create a custom (Starlark) plugin. Plugins are immutable, so there is
    /// no replace counterpart.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `403` for a tenantless caller, `409` when
    /// `(tenant_id, name)` is taken.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        input: PluginInput,
    ) -> Result<Plugin, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        let kind = input
            .kind
            .ok_or_else(|| OagwError::validation("plugin_type is required"))?;
        let name = input
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| OagwError::validation("name is required"))?
            .to_owned();
        let source_code = input
            .source_code
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| OagwError::validation("source_code is required"))?;
        if let Some(schema) = &input.config_schema
            && !schema.is_object()
        {
            return Err(OagwError::validation(
                "config_schema must be a JSON Schema object",
            ));
        }
        if kind != PluginKind::Transform && !input.phases.is_empty() {
            return Err(OagwError::validation(
                "phases may only be declared by a transform plugin",
            ));
        }
        self.plugins
            .insert(Plugin {
                id: Uuid::new_v4(),
                tenant_id,
                kind,
                name,
                description: input.description,
                phases: input.phases,
                config_schema: input.config_schema,
                source_code,
                created_at: timeutil::now_rfc3339(),
                last_used_at: None,
                gc_eligible_at: None,
            })
            .await
    }

    /// Fetch a plugin owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller, `404` when not visible.
    pub async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<Plugin, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        self.plugins
            .get(tenant_id, id)
            .await
            .ok_or_else(|| OagwError::not_found(format!("plugin '{id}' not found")))
    }

    /// List the calling tenant's plugins.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller.
    pub async fn list_plugins(&self, ctx: &SecurityContext) -> Result<Vec<Plugin>, OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        Ok(self.plugins.list(tenant_id).await)
    }

    /// Delete a plugin. Only unlinked plugins may be deleted.
    ///
    /// # Errors
    ///
    /// `403` for a tenantless caller, `404` when not visible, `409` with the
    /// referencing upstreams and routes when the plugin is still bound.
    pub async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), OagwError> {
        let tenant_id = Self::tenant_of(ctx)?;
        let plugin = self
            .plugins
            .get(tenant_id, id)
            .await
            .ok_or_else(|| OagwError::not_found(format!("plugin '{id}' not found")))?;
        let references = self.plugin_references(id).await;
        if !references.is_empty() {
            let upstream_ids: Vec<Value> = references
                .iter()
                .filter_map(|r| match r {
                    PluginReference::Upstream(uuid) => {
                        Some(Value::from(gts::anonymous_id(gts::UPSTREAM_TYPE, *uuid)))
                    }
                    PluginReference::Route(_) => None,
                })
                .collect();
            let route_ids: Vec<Value> = references
                .iter()
                .filter_map(|r| match r {
                    PluginReference::Route(uuid) => {
                        Some(Value::from(gts::anonymous_id(gts::ROUTE_TYPE, *uuid)))
                    }
                    PluginReference::Upstream(_) => None,
                })
                .collect();
            let mut referenced_by = Map::new();
            referenced_by.insert("upstreams".to_owned(), Value::Array(upstream_ids.clone()));
            referenced_by.insert("routes".to_owned(), Value::Array(route_ids.clone()));
            return Err(OagwError::plugin_in_use(format!(
                "Plugin is referenced by {} upstream(s) and {} route(s)",
                upstream_ids.len(),
                route_ids.len()
            ))
            .with("plugin_id", plugin.gts_id())
            .with("referenced_by", Value::Object(referenced_by)));
        }
        if self.plugins.delete(tenant_id, id).await {
            Ok(())
        } else {
            Err(OagwError::not_found(format!("plugin '{id}' not found")))
        }
    }

    /// Every upstream and route that binds `plugin_id`, in that order.
    pub async fn plugin_references(&self, plugin_id: Uuid) -> Vec<PluginReference> {
        let mut refs = Vec::new();
        for upstream in self.upstreams.all().await {
            let auth_hit = upstream
                .auth_plugin_ref()
                .and_then(gts::plugin_ref_uuid)
                .is_some_and(|uuid| uuid == plugin_id);
            let chain_hit = upstream.spec.plugins.as_ref().is_some_and(|p| {
                p.items
                    .iter()
                    .any(|b| gts::plugin_ref_uuid(&b.plugin_ref) == Some(plugin_id))
            });
            if auth_hit || chain_hit {
                refs.push(PluginReference::Upstream(upstream.id));
            }
        }
        for route in self.routes.all().await {
            let hit = route.spec.plugins.as_ref().is_some_and(|p| {
                p.items
                    .iter()
                    .any(|b| gts::plugin_ref_uuid(&b.plugin_ref) == Some(plugin_id))
            });
            if hit {
                refs.push(PluginReference::Route(route.id));
            }
        }
        refs
    }

    /// Every custom plugin id currently bound anywhere — the GC input set.
    pub async fn linked_plugin_ids(&self) -> Vec<Uuid> {
        let mut linked = Vec::new();
        for upstream in self.upstreams.all().await {
            if let Some(uuid) = upstream.auth_plugin_ref().and_then(gts::plugin_ref_uuid) {
                linked.push(uuid);
            }
            if let Some(plugins) = &upstream.spec.plugins {
                linked.extend(
                    plugins
                        .items
                        .iter()
                        .filter_map(|b| gts::plugin_ref_uuid(&b.plugin_ref)),
                );
            }
        }
        for route in self.routes.all().await {
            if let Some(plugins) = &route.spec.plugins {
                linked.extend(
                    plugins
                        .items
                        .iter()
                        .filter_map(|b| gts::plugin_ref_uuid(&b.plugin_ref)),
                );
            }
        }
        linked.sort();
        linked.dedup();
        linked
    }
}

/// Where a plugin binding was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginReference {
    /// Bound by an upstream (as `auth.type` or in its chain).
    Upstream(Uuid),
    /// Bound by a route's chain.
    Route(Uuid),
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut normalized: Vec<String> = tags
        .into_iter()
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();
    normalized
}

#[cfg(test)]
#[path = "management_tests.rs"]
mod tests;
