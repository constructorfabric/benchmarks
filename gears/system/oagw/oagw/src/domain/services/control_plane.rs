//! Control-plane service: CRUD for upstreams, routes and custom plugins.
//!
//! Every mutation is tenant-scoped and PEP-authorized. The management
//! plane sees only the calling tenant's own resources (DOCS §1.2) —
//! ancestor resources are resolved at proxy time, never listed here.
//!
//! Authorization mirrors the AM `authz_scope` gate: an `AccessRequest`
//! carrying `OWNER_TENANT_ID` (and `RESOURCE_ID` when the operation
//! targets a row), `require_constraints(true)`, evaluated through the
//! [`PolicyEnforcer`]. Denied / un-compilable decisions surface as
//! `403 Forbidden`.

use std::sync::Arc;

use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer, ResourceType};
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient, TenantResolverError};
use toolkit_security::{pep_properties, AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{
    Endpoint, Plugin, PluginInput, PluginKind, PluginRef, PluginsConfig, RateLimitConfig, Route,
    RouteInput, RouteMatch, Upstream, UpstreamInput,
};
use crate::domain::repo::{PluginRepo, RouteRepo, UpstreamRepo};
use crate::domain::services::alias::{is_ip_literal, resolve_alias};
use crate::domain::services::list::{apply, ListQuery};
use crate::gts_helpers;

// ---------------------------------------------------------------------------
// PEP resource descriptors + actions
// ---------------------------------------------------------------------------

/// Upstream resource type (for PDP constraint compilation).
pub const UPSTREAM_RESOURCE: ResourceType =
    ResourceType::from_static(gts_helpers::UPSTREAM_TYPE_ID, &["owner_tenant_id", "id"]);
/// Route resource type.
pub const ROUTE_RESOURCE: ResourceType =
    ResourceType::from_static(gts_helpers::ROUTE_TYPE_ID, &["owner_tenant_id", "id"]);
/// Custom plugin resource type.
pub const PLUGIN_RESOURCE: ResourceType =
    ResourceType::from_static(gts_helpers::PLUGIN_TYPE_ID, &["owner_tenant_id", "id"]);
/// Proxy resource (data-plane invocation).
pub const PROXY_RESOURCE: ResourceType =
    ResourceType::from_static(gts_helpers::PROXY_TYPE_ID, &["owner_tenant_id", "alias"]);

/// PEP action name for creating a resource.
pub const ACTION_CREATE: &str = "create";
/// PEP action name for reading a resource.
pub const ACTION_READ: &str = "read";
/// PEP action name for replacing a resource (`PUT`).
pub const ACTION_OVERRIDE: &str = "override";
/// PEP action name for deleting a resource.
pub const ACTION_DELETE: &str = "delete";
/// PEP action name for binding a descendant resource to an ancestor alias.
pub const ACTION_BIND: &str = "bind";
/// PEP action name for data-plane invocation.
pub const ACTION_INVOKE: &str = "invoke";

/// Instantiable builtin auth plugin GTS ids (catalog-only ids are
/// rejected during validation — DOCS §3.3).
pub const INSTANTIABLE_AUTH_PLUGINS: [&str; 4] = [
    gts_helpers::AUTH_PLUGIN_NOOP,
    gts_helpers::AUTH_PLUGIN_APIKEY,
    gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED,
    gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
];
/// Instantiable builtin guard plugin GTS ids.
pub const INSTANTIABLE_GUARD_PLUGINS: [&str; 1] = [gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS];
/// Instantiable builtin transform plugin GTS ids.
pub const INSTANTIABLE_TRANSFORM_PLUGINS: [&str; 1] = [gts_helpers::TRANSFORM_PLUGIN_REQUEST_ID];

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Control-plane service behind the management REST API.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepo>,
    routes: Arc<dyn RouteRepo>,
    plugins: Arc<dyn PluginRepo>,
    enforcer: Arc<PolicyEnforcer>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
    allow_http_upstream: bool,
}

impl ControlPlaneService {
    /// Build the service over the given repositories and PEP.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepo>,
        routes: Arc<dyn RouteRepo>,
        plugins: Arc<dyn PluginRepo>,
        enforcer: Arc<PolicyEnforcer>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            enforcer,
            tenant_resolver,
            allow_http_upstream,
        }
    }

    // -- PEP gate -----------------------------------------------------------

    /// Compile `(resource, action)` for the caller into an
    /// [`AccessScope`]. Fail-closed: denied or unconstrainable decisions
    /// surface as 403.
    async fn authorize(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &str,
        resource_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .require_constraints(true);
        if let Some(rid) = resource_id {
            request = request.resource_property(pep_properties::RESOURCE_ID, rid);
        }
        self.enforcer
            .access_scope_with(ctx, resource, action, resource_id, &request)
            .await
            .map_err(DomainError::from)
    }

    /// Tenant chain ordered descendant → root, starting with the caller.
    async fn tenant_chain(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Uuid>, DomainError> {
        let tid = ctx.subject_tenant_id();
        let mut chain = vec![tid];
        let resp = self
            .tenant_resolver
            .get_ancestors(ctx, TenantId(tid), &GetAncestorsOptions::default())
            .await
            .map_err(|e| map_tenant_error(&e))?;
        for ancestor in resp.ancestors {
            chain.push(ancestor.id.0);
        }
        Ok(chain)
    }

    // -- Upstream CRUD ------------------------------------------------------

    /// Validate + normalize an upstream definition; assign id / alias.
    async fn validate_upstream(
        &self,
        ctx: &SecurityContext,
        input: &UpstreamInput,
    ) -> Result<String, DomainError> {
        if input.server.endpoints.is_empty() {
            return Err(DomainError::validation(
                "upstream must declare at least one endpoint",
            ));
        }
        for e in &input.server.endpoints {
            if e.scheme == crate::domain::models::EndpointScheme::Http && !self.allow_http_upstream {
                return Err(DomainError::validation(
                    "upstream http endpoints are disabled; configure allow_http_upstream",
                ));
            }
        }
        let (alias, _changed) = resolve_alias(&input.server.endpoints, input.alias.as_deref())?;
        self.validate_plugin_chain(ctx, input.plugins.as_ref()).await?;
        Self::validate_rate_limit(input.rate_limit.as_ref())?;
        Ok(alias)
    }

    /// Create an upstream owned by the calling tenant.
    ///
    /// # Errors
    /// * 400 when the definition violates the alias/endpoint/plugin rules.
    /// * 409 when the (tenant-scoped) alias is already taken.
    /// * 403 when the PEP denies creation.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamInput,
    ) -> Result<Upstream, DomainError> {
        self.authorize(ctx, &UPSTREAM_RESOURCE, ACTION_CREATE, None).await?;

        let alias = self.validate_upstream(ctx, &input).await?;

        // A descendant may bind an ancestor's alias (DOCS §4.2); that
        // requires the `bind` permission and is not a conflict.
        if self.upstreams.alias_exists(ctx.subject_tenant_id(), &alias).await {
            return Err(DomainError::already_exists("upstream", format!("alias '{alias}' is already in use")));
        }
        let bound = self.ancestor_uses_alias(ctx, &alias).await?;
        if bound {
            self.authorize(ctx, &UPSTREAM_RESOURCE, ACTION_BIND, None).await?;
        }

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            enabled: input.enabled,
            alias,
            tags: input.tags,
            server: input.server,
            protocol: input.protocol,
            auth: input.auth,
            headers: input.headers,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
        };
        self.upstreams.insert(upstream.clone()).await?;
        Ok(upstream)
    }

    /// Fetch an upstream owned by the calling tenant.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    /// * 403 when the PEP denies the read.
    pub async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, DomainError> {
        self.authorize(ctx, &UPSTREAM_RESOURCE, ACTION_READ, Some(id)).await?;
        self.upstreams.get(ctx.subject_tenant_id(), id).await
    }

    /// List upstreams owned by the calling tenant, filtered + paged.
    ///
    /// # Errors
    /// * 403 when the PEP denies the read.
    pub async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, DomainError> {
        self.authorize(ctx, &UPSTREAM_RESOURCE, ACTION_READ, None).await?;
        let items = self.upstreams.list(ctx.subject_tenant_id()).await;
        Ok(apply(items, query, |u| serde_json::to_value(u).unwrap_or_default()))
    }

    /// Replace an upstream owned by the calling tenant in full.
    ///
    /// The alias is immutable: the new endpoints must derive the same
    /// alias (or be an all-IP set keeping the explicit alias).
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    /// * 400 on validation failures, including alias changes.
    /// * 403 when the PEP denies.
    pub async fn update_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, DomainError> {
        self.authorize(ctx, &UPSTREAM_RESOURCE, ACTION_OVERRIDE, Some(id)).await?;
        let existing = self.upstreams.get(ctx.subject_tenant_id(), id).await?;
        let alias = self.validate_upstream(ctx, &input).await?;
        Self::enforce_alias_immutability(&existing, &input.server.endpoints, &alias)?;
        self.upstreams
            .replace(
                ctx.subject_tenant_id(),
                Upstream {
                    id,
                    tenant_id: ctx.subject_tenant_id(),
                    enabled: input.enabled,
                    alias,
                    tags: input.tags,
                    server: input.server,
                    protocol: input.protocol,
                    auth: input.auth,
                    headers: input.headers,
                    plugins: input.plugins,
                    rate_limit: input.rate_limit,
                    cors: input.cors,
                },
            )
            .await?;
        self.upstreams.get(ctx.subject_tenant_id(), id).await
    }

    /// Delete an upstream owned by the calling tenant.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    /// * 409 when routes in the tenant reference the upstream.
    /// * 403 when the PEP denies.
    pub async fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), DomainError> {
        self.authorize(ctx, &UPSTREAM_RESOURCE, ACTION_DELETE, Some(id)).await?;
        let existing = self.upstreams.get(ctx.subject_tenant_id(), id).await?;
        let referencing: Vec<String> = self
            .routes
            .list(ctx.subject_tenant_id())
            .await
            .into_iter()
            .filter(|r| r.upstream_id == id)
            .map(|r| r.id.to_string())
            .collect();
        if !referencing.is_empty() {
            return Err(DomainError::aborted(format!(
                "cannot delete upstream '{}' (alias '{}'): {} route(s) reference it",
                existing.id,
                existing.alias,
                referencing.len()
            )));
        }
        self.upstreams.delete(ctx.subject_tenant_id(), id).await
    }

    /// Whether any ancestor in the caller's chain exposes `alias`.
    async fn ancestor_uses_alias(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> Result<bool, DomainError> {
        let chain = self.tenant_chain(ctx).await?;
        for ancestor in chain.iter().skip(1) {
            if self
                .upstreams
                .resolve_alias(std::slice::from_ref(ancestor), alias)
                .await
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The alias is immutable across `PUT`: reject any update that would
    /// change the effective alias value.
    fn enforce_alias_immutability(
        existing: &Upstream,
        new_endpoints: &[Endpoint],
        resolved: &str,
    ) -> Result<(), DomainError> {
        if resolved == existing.alias {
            return Ok(());
        }
        // All-IP endpoint sets keep their explicit (required) alias.
        let existing_ips = existing
            .server
            .endpoints
            .iter()
            .all(|e| is_ip_literal(&e.host));
        let new_ips = new_endpoints.iter().all(|e| is_ip_literal(&e.host));
        if existing_ips && new_ips {
            return Ok(());
        }
        Err(DomainError::validation(format!(
            "alias is immutable: the declared endpoints resolve to '{resolved}' but the upstream \
             alias is '{}'; delete and recreate the upstream to change its alias",
            existing.alias
        )))
    }

    // -- Route CRUD ---------------------------------------------------------

    /// Validate a route definition (upstream ownership + match rules).
    async fn validate_route(
        &self,
        ctx: &SecurityContext,
        input: &RouteInput,
    ) -> Result<(), DomainError> {
        // The referenced upstream must exist and be owned by the caller.
        match self.upstreams.get_any_tenant(input.upstream_id).await {
            None => {
                return Err(DomainError::validation(format!(
                    "route references unknown upstream '{}'",
                    input.upstream_id
                )));
            }
            Some(u) if u.tenant_id != ctx.subject_tenant_id() => {
                return Err(DomainError::not_found(
                    "upstream",
                    format!(
                        "upstream '{}' is not visible to the calling tenant",
                        input.upstream_id
                    ),
                ));
            }
            Some(_) => {}
        }
        Self::validate_match(&input.match_)?;
        self.validate_plugin_chain(ctx, input.plugins.as_ref()).await?;
        Self::validate_rate_limit(input.rate_limit.as_ref())?;
        Ok(())
    }

    /// Match block must have exactly one of `http` / `grpc`, non-empty.
    fn validate_match(match_: &RouteMatch) -> Result<(), DomainError> {
        let (way, name) = match (&match_.http, &match_.grpc) {
            (Some(h), None) => {
                if h.methods.is_empty() {
                    return Err(DomainError::validation(
                        "route match.http.methods must not be empty",
                    ));
                }
                if !h.path.starts_with('/') {
                    return Err(DomainError::validation(format!(
                        "route match.http.path '{}' must start with '/'",
                        h.path
                    )));
                }
                ((), "http".to_owned())
            }
            (None, Some(g)) => {
                if g.service.is_empty() || g.method.is_empty() {
                    return Err(DomainError::validation(
                        "route match.grpc.service and .method must be non-empty",
                    ));
                }
                ((), "grpc".to_owned())
            }
            (Some(_), Some(_)) => {
                return Err(DomainError::validation(
                    "route match must declare exactly one of http or grpc",
                ));
            }
            (None, None) => {
                return Err(DomainError::validation(
                    "route match must declare http or grpc",
                ));
            }
        };
        let _ = (way, name);
        Ok(())
    }

    /// Create a route owned by the calling tenant.
    ///
    /// # Errors
    /// * 400 on match/plugin errors or an unknown upstream.
    /// * 404 when the upstream is not visible.
    /// * 409 on a match collision within the upstream.
    /// * 403 when the PEP denies.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        input: RouteInput,
    ) -> Result<Route, DomainError> {
        self.authorize(ctx, &ROUTE_RESOURCE, ACTION_CREATE, None).await?;
        self.validate_route(ctx, &input).await?;
        Self::check_route_collisions(
            &self.routes.list(ctx.subject_tenant_id()).await,
            &input,
            None,
        )?;
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            tags: input.tags,
            upstream_id: input.upstream_id,
            match_: input.match_,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
        };
        self.routes.insert(route.clone()).await?;
        Ok(route)
    }

    /// Fetch a route owned by the calling tenant.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    pub async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<Route, DomainError> {
        self.authorize(ctx, &ROUTE_RESOURCE, ACTION_READ, Some(id)).await?;
        self.routes.get(ctx.subject_tenant_id(), id).await
    }

    /// List routes owned by the calling tenant, filtered + paged.
    ///
    /// # Errors
    /// * 403 when the PEP denies the read.
    pub async fn list_routes(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Route>, DomainError> {
        self.authorize(ctx, &ROUTE_RESOURCE, ACTION_READ, None).await?;
        let items = self.routes.list(ctx.subject_tenant_id()).await;
        Ok(apply(items, query, |r| serde_json::to_value(r).unwrap_or_default()))
    }

    /// Replace a route owned by the calling tenant in full.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    /// * 409 on a match collision (excluding self).
    /// * 403 when the PEP denies.
    pub async fn update_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, DomainError> {
        self.authorize(ctx, &ROUTE_RESOURCE, ACTION_OVERRIDE, Some(id)).await?;
        self.routes.get(ctx.subject_tenant_id(), id).await?;
        self.validate_route(ctx, &input).await?;
        Self::check_route_collisions(
            &self.routes.list(ctx.subject_tenant_id()).await,
            &input,
            Some(id),
        )?;
        self.routes
            .replace(
                ctx.subject_tenant_id(),
                Route {
                    id,
                    tenant_id: ctx.subject_tenant_id(),
                    tags: input.tags,
                    upstream_id: input.upstream_id,
                    match_: input.match_,
                    plugins: input.plugins,
                    rate_limit: input.rate_limit,
                    cors: input.cors,
                },
            )
            .await?;
        self.routes.get(ctx.subject_tenant_id(), id).await
    }

    /// Delete a route owned by the calling tenant.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    /// * 403 when the PEP denies.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        self.authorize(ctx, &ROUTE_RESOURCE, ACTION_DELETE, Some(id)).await?;
        self.routes.delete(ctx.subject_tenant_id(), id).await
    }

    /// Detect a match collision against an existing route list.
    /// `exclude` skips the route being replaced.
    fn check_route_collisions(
        existing: &[Route],
        input: &RouteInput,
        exclude: Option<Uuid>,
    ) -> Result<(), DomainError> {
        for r in existing {
            if exclude == Some(r.id) {
                continue;
            }
            if r.upstream_id != input.upstream_id {
                continue;
            }
            match (&r.match_.http, &input.match_.http) {
                (Some(a), Some(b))
                    if a.path == b.path
                        && a.path_suffix_mode == b.path_suffix_mode
                        && a.methods.iter().any(|m| b.methods.contains(m)) =>
                {
                    return Err(DomainError::already_exists(
                        "route",
                        format!(
                            "a route matching path '{}' already exists on upstream '{}'",
                            a.path, input.upstream_id
                        ),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    // -- Custom plugin CRUD -------------------------------------------------

    /// Create a custom plugin definition owned by the calling tenant.
    ///
    /// # Errors
    /// * 400 when `kind`/`builtin_type` mismatch or the builtin is unknown.
    /// * 409 when the (tenant-scoped) name is already taken.
    /// * 403 when the PEP denies.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        input: PluginInput,
    ) -> Result<Plugin, DomainError> {
        self.authorize(ctx, &PLUGIN_RESOURCE, ACTION_CREATE, None).await?;
        Self::validate_plugin_input(&input)?;
        if self
            .plugins
            .list(ctx.subject_tenant_id())
            .await
            .iter()
            .any(|p| p.name == input.name)
        {
            return Err(DomainError::already_exists(
                "plugin",
                format!("plugin name '{}' is already in use", input.name),
            ));
        }
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            kind: input.kind,
            builtin_type: input.builtin_type,
            name: input.name,
            config: input.config,
        };
        self.plugins.insert(plugin.clone()).await?;
        Ok(plugin)
    }

    /// Fetch a custom plugin owned by the calling tenant.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    pub async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<Plugin, DomainError> {
        self.authorize(ctx, &PLUGIN_RESOURCE, ACTION_READ, Some(id)).await?;
        self.plugins.get(ctx.subject_tenant_id(), id).await
    }

    /// List custom plugins owned by the calling tenant, filtered + paged.
    ///
    /// # Errors
    /// * 403 when the PEP denies the read.
    pub async fn list_plugins(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Plugin>, DomainError> {
        self.authorize(ctx, &PLUGIN_RESOURCE, ACTION_READ, None).await?;
        let items = self.plugins.list(ctx.subject_tenant_id()).await;
        Ok(apply(items, query, |p| serde_json::to_value(p).unwrap_or_default()))
    }

    /// Delete a custom plugin owned by the calling tenant.
    ///
    /// # Errors
    /// * 404 when absent or owned by another tenant.
    /// * 409 when upstreams or routes still reference the plugin.
    /// * 403 when the PEP denies.
    pub async fn delete_plugin(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), DomainError> {
        self.authorize(ctx, &PLUGIN_RESOURCE, ACTION_DELETE, Some(id)).await?;
        let _existing = self.plugins.get(ctx.subject_tenant_id(), id).await?;

        let mut upstreams = Vec::new();
        let mut routes = Vec::new();
        for u in self.upstreams.all_upstreams().await {
            if items_reference(u.plugins.as_ref(), id) {
                upstreams.push(u.alias);
            }
        }
        for r in self.routes.all_routes().await {
            if items_reference(r.plugins.as_ref(), id) {
                routes.push(r.id.to_string());
            }
        }
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(DomainError::plugin_in_use(
                id,
                crate::domain::error::PluginReferences { upstreams, routes },
            ));
        }
        self.plugins.delete(ctx.subject_tenant_id(), id).await
    }

    // -- Shared validation --------------------------------------------------

    /// Validate the plugin chain: builtin ids must be instantiable
    /// (catalog-only ids are "unknown plugin"), custom uuids must exist
    /// and be owned by the caller.
    async fn validate_plugin_chain(
        &self,
        ctx: &SecurityContext,
        chain: Option<&PluginsConfig>,
    ) -> Result<(), DomainError> {
        let Some(chain) = chain else { return Ok(()) };
        for item in &chain.items {
            match item.plugin_ref() {
                PluginRef::BuiltinId(id) => {
                    if !known_builtin(id) {
                        return Err(crate::domain::error::DomainError::validation(format!(
                            "unknown plugin '{id}'"
                        )));
                    }
                }
                PluginRef::Custom(uuid) => {
                    match self.plugins.get_any_tenant(*uuid).await {
                        None => {
                            return Err(DomainError::validation(format!(
                                "unknown plugin '{uuid}'"
                            )));
                        }
                        Some(p) if p.tenant_id != ctx.subject_tenant_id() => {
                            return Err(DomainError::validation(format!(
                                "plugin '{uuid}' belongs to another tenant"
                            )));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Basic rate-limit sanity: sustained rate > 0.
    fn validate_rate_limit(rl: Option<&RateLimitConfig>) -> Result<(), DomainError> {
        if rl.is_some_and(|rl| rl.sustained.rate == 0) {
            return Err(DomainError::validation(
                "rate_limit.sustained.rate must be > 0",
            ));
        }
        Ok(())
    }

    /// Validate a plugin definition: builtin id must be known AND match
    /// the declared kind.
    fn validate_plugin_input(input: &PluginInput) -> Result<(), DomainError> {
        let known = match input.kind {
            PluginKind::Auth => INSTANTIABLE_AUTH_PLUGINS.contains(&input.builtin_type.as_str()),
            PluginKind::Guard => {
                INSTANTIABLE_GUARD_PLUGINS.contains(&input.builtin_type.as_str())
            }
            PluginKind::Transform => {
                INSTANTIABLE_TRANSFORM_PLUGINS.contains(&input.builtin_type.as_str())
            }
        };
        let kind = match input.kind {
            PluginKind::Auth => "auth",
            PluginKind::Guard => "guard",
            PluginKind::Transform => "transform",
        };
        if !known {
            return Err(DomainError::validation(format!(
                "unknown plugin '{}' for kind '{}'",
                input.builtin_type, kind
            )));
        }
        Ok(())
    }
}

/// Whether a plugin chain references the given custom plugin uuid.
fn items_reference(chain: Option<&PluginsConfig>, id: Uuid) -> bool {
    chain
        .as_ref()
        .is_some_and(|c| c.items.iter().any(|i| matches!(i.plugin_ref(), PluginRef::Custom(u) if *u == id)))
}

/// Whether `id` is an instantiable builtin (vs a catalog-only id).
#[must_use]
pub fn known_builtin(id: &str) -> bool {
    INSTANTIABLE_AUTH_PLUGINS.contains(&id)
        || INSTANTIABLE_GUARD_PLUGINS.contains(&id)
        || INSTANTIABLE_TRANSFORM_PLUGINS.contains(&id)
}

/// Map a tenant-resolver transport failure onto the domain error model.
fn map_tenant_error(err: &TenantResolverError) -> DomainError {
    DomainError::resolve_failed(format!("tenant hierarchy lookup failed: {err}"))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{
        Endpoint, EndpointScheme, HttpMatch, HttpMethod, PathSuffixMode, RouteMatch, ServerConfig,
        UpstreamProtocol,
    };

    fn ep(host: &str) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port: None,
        }
    }

    fn route_input(upstream_id: Uuid, path: &str, methods: &[&str]) -> RouteInput {
        RouteInput {
            tags: vec![],
            upstream_id,
            match_: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods
                        .iter()
                        .map(|m| match *m {
                            "POST" => HttpMethod::Post,
                            "PUT" => HttpMethod::Put,
                            "PATCH" => HttpMethod::Patch,
                            "DELETE" => HttpMethod::Delete,
                            "HEAD" => HttpMethod::Head,
                            "OPTIONS" => HttpMethod::Options,
                            _ => HttpMethod::Get,
                        })
                        .collect(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn alias_immutability_hostname_to_hostname_rejects() {
        let existing = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: "api.openai.com".to_owned(),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![ep("api.openai.com")],
            },
            protocol: UpstreamProtocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        };
        // Different hostname pool → different alias → rejected.
        let err = ControlPlaneService::enforce_alias_immutability(
            &existing,
            &[ep("other.vendor.com")],
            "other.vendor.com",
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));
    }

    #[test]
    fn alias_immutability_ips_keep_explicit_alias() {
        let existing = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: "my-internal-service".to_owned(),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![ep("10.0.1.1")],
            },
            protocol: UpstreamProtocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        };
        // IP pool → explicit alias stays.
        let res = ControlPlaneService::enforce_alias_immutability(
            &existing,
            &[ep("10.0.1.2"), ep("10.0.1.3")],
            "my-internal-service",
        );
        assert!(res.is_ok());
    }

    #[test]
    fn known_builtin_classification() {
        assert!(known_builtin(gts_helpers::AUTH_PLUGIN_APIKEY));
        assert!(known_builtin(gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS));
        assert!(known_builtin(gts_helpers::TRANSFORM_PLUGIN_REQUEST_ID));
        // Catalog-only (unbindable) ids are NOT instantiable.
        assert!(!known_builtin(gts_helpers::AUTH_PLUGIN_BASIC_CATALOG));
        assert!(!known_builtin(gts_helpers::GUARD_PLUGIN_CORS_CATALOG));
        assert!(!known_builtin(gts_helpers::TRANSFORM_PLUGIN_LOGGING_CATALOG));
    }

    #[test]
    fn validate_match_requires_exactly_one() {
        // Both http + grpc → rejected.
        let err = ControlPlaneService::validate_match(&RouteMatch {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/x".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: Some(crate::domain::models::GrpcMatch {
                service: "svc".to_owned(),
                method: "m".to_owned(),
            }),
        })
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));

        // Neither → rejected.
        let err =
            ControlPlaneService::validate_match(&RouteMatch { http: None, grpc: None }).unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));

        // http without leading '/' → rejected.
        let err = ControlPlaneService::validate_match(&RouteMatch {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "no-slash".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        })
        .unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));
    }

    #[test]
    fn route_collision_detection() {
        let upstream_id = Uuid::new_v4();
        let existing = vec![Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            tags: vec![],
            upstream_id,
            match_: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: "/v1/chat".to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        }];

        // Same path + overlapping method → conflict.
        let err = ControlPlaneService::check_route_collisions(
            &existing,
            &route_input(upstream_id, "/v1/chat", &["GET", "POST"]),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DomainError::AlreadyExists { .. }));

        // Different path → fine.
        assert!(
            ControlPlaneService::check_route_collisions(
                &existing,
                &route_input(upstream_id, "/v1/other", &["GET"]),
                None,
            )
            .is_ok()
        );

        // Different upstream → fine.
        assert!(
            ControlPlaneService::check_route_collisions(
                &existing,
                &route_input(Uuid::new_v4(), "/v1/chat", &["GET"]),
                None,
            )
            .is_ok()
        );

        // Replacing the colliding route itself → fine.
        assert!(
            ControlPlaneService::check_route_collisions(
                &existing,
                &route_input(upstream_id, "/v1/chat", &["GET"]),
                Some(existing[0].id),
            )
            .is_ok()
        );
    }
}
