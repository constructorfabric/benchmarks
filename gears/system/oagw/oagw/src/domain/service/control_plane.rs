//! Control Plane service — tenant-scoped management CRUD for upstreams,
//! routes, and plugins (feature `cpt-cf-oagw-feature-control-plane-api`,
//! flows `cpt-cf-oagw-flow-control-plane-api-{upstream,route,plugin}-crud`).
//!
//! Distinct domain-service boundary per DoD
//! `cpt-cf-oagw-dod-gear-foundation-skeleton`.  The Control Plane owns the
//! management invariants (algorithm `cpt-cf-oagw-algo-control-plane-api-*`):
//!
//! - **Authorization** — every operation is gated on the GTS permission set
//!   `gts.cf.core.oagw.{upstream|route|auth_plugin|guard_plugin|transform_plugin}.v1~:{create;override;read;delete}`
//!   through `authz_resolver` (DoD
//!   `cpt-cf-oagw-dod-control-plane-api-authz`, algorithm
//!   `cpt-cf-oagw-algo-control-plane-api-authorize`); a denial is 403
//!   `access.denied` (`gts.cf.core.errors.err.v1~cf.oagw.access.denied.v1`),
//!   an evaluation failure is 503 `service.unavailable`.
//! - **Alias derivation/enforcement** — hostname pools auto-derive their
//!   alias (single hostname; longest common suffix ≥ 2 labels validated
//!   against the PSL); IP-based and non-derivable pools require an explicit
//!   alias; normalization is ASCII-lowercase with trailing dots stripped;
//!   alias is immutable after creation (algorithm
//!   `cpt-cf-oagw-algo-control-plane-api-derive-alias`, DoD
//!   `cpt-cf-oagw-dod-control-plane-api-alias`).
//! - **Uniqueness** — `(tenant_id, alias)` conflicts surface as 409
//!   `upstream.alias_conflict` (algorithm
//!   `cpt-cf-oagw-algo-control-plane-api-validate-alias`); route
//!   `(path_prefix, priority)` collisions as 409 `route.conflict`
//!   (algorithm `cpt-cf-oagw-algo-control-plane-api-validate-match-rule`);
//!   plugin `(tenant_id, name)` conflicts as 409 `plugin.conflict`.
//! - **Tenant scoping / ancestor 404** — reads/replaces/deletes are
//!   tenant-scoped by the subject tenant; ancestor-owned resources are never
//!   visible through the management surface (DoD
//!   `cpt-cf-oagw-dod-control-plane-api-tenant-scope`).
//!
//! The service consumes domain "draft" types ([`UpstreamDraft`],
//! [`RouteDraft`]) so the domain layer never depends on the API DTOs; the
//! `api/rest` layer converts request DTOs into drafts (validation per
//! `cpt-cf-oagw-algo-control-plane-api-validate-dto`) and response entities
//! into view DTOs.

use std::sync::Arc;

use authz_resolver_sdk::AuthZResolverClient;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use tenant_resolver_sdk::TenantResolverClient;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::entity::ServerConfig;
use crate::domain::entity::alias::{
    compute_derived_alias, normalize_alias, requires_explicit_alias, validate_alias,
    validate_endpoint_url,
};
use crate::domain::entity::config::{
    CorsConfig, HeadersConfig, PluginBinding, PluginsConfig, RateLimitConfig, SharingMode,
    UpstreamProtocol,
};
use crate::domain::entity::plugin::{Plugin, PluginType};
use crate::domain::entity::route::{MatchType, Route, RouteMatch};
use crate::domain::entity::upstream::{AuthConfig, Endpoint, Upstream};
use crate::domain::error::DomainError;
use crate::domain::repo::http_match_of;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// GTS resource types and actions for management authorization (DESIGN §3.3,
/// DoD `cpt-cf-oagw-dod-control-plane-api-authz`).  Resource names are the
/// instance-prefix form `gts.cf.core.oagw.{type}.v1~`; the action set is
/// `create|override|read|delete` (note `override`, not `update`).
pub mod authz {
    use authz_resolver_sdk::pep::ResourceType;
    use toolkit_security::pep_properties;

    /// `gts.cf.core.oagw.upstream.v1~` resource type.
    pub const UPSTREAM: ResourceType = ResourceType::from_static(
        "gts.cf.core.oagw.upstream.v1~",
        &[pep_properties::OWNER_TENANT_ID],
    );
    /// `gts.cf.core.oagw.route.v1~` resource type.
    pub const ROUTE: ResourceType = ResourceType::from_static(
        "gts.cf.core.oagw.route.v1~",
        &[pep_properties::OWNER_TENANT_ID],
    );
    /// `gts.cf.core.oagw.auth_plugin.v1~` resource type.
    pub const AUTH_PLUGIN: ResourceType = ResourceType::from_static(
        "gts.cf.core.oagw.auth_plugin.v1~",
        &[pep_properties::OWNER_TENANT_ID],
    );
    /// `gts.cf.core.oagw.guard_plugin.v1~` resource type.
    pub const GUARD_PLUGIN: ResourceType = ResourceType::from_static(
        "gts.cf.core.oagw.guard_plugin.v1~",
        &[pep_properties::OWNER_TENANT_ID],
    );
    /// `gts.cf.core.oagw.transform_plugin.v1~` resource type.
    pub const TRANSFORM_PLUGIN: ResourceType = ResourceType::from_static(
        "gts.cf.core.oagw.transform_plugin.v1~",
        &[pep_properties::OWNER_TENANT_ID],
    );

    /// Management actions on the resource types above.
    pub mod actions {
        /// Create a resource.
        pub const CREATE: &str = "create";
        /// Replace (PUT) an existing resource.
        pub const OVERRIDE: &str = "override";
        /// Read (GET/list) a resource.
        pub const READ: &str = "read";
        /// Delete a resource.
        pub const DELETE: &str = "delete";
    }
}

/// Maps a PEP enforcement failure to a domain error (fail-closed): `Denied`
/// or `CompileFailed` → 403 `access.denied`; `EvaluationFailed` → 503
/// `service.unavailable` (algorithm
/// `cpt-cf-oagw-algo-control-plane-api-authorize`, steps
/// `inst-cp-authz-deny`).
#[must_use]
pub fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::AccessDenied {
                detail: "caller lacks the required permission for this management operation"
                    .to_owned(),
                cause: None,
            }
        }
        EnforcerError::EvaluationFailed(source) => DomainError::ServiceUnavailable {
            detail: "authorization evaluation failed".to_owned(),
            retry_after: None,
            cause: Some(Box::new(EnforcerError::EvaluationFailed(source))),
        },
    }
}

/// Domain "draft" for creating/replacing an upstream: everything the
/// `api/rest` layer extracted from the request DTO, with no dependency on the
/// API types.  `plugin_refs` carry either a built-in plugin GTS reference (or
/// bare tier) or a `{uuid}` of a tenant-created custom plugin (DoD
/// `cpt-cf-oagw-dod-control-plane-api-plugin-crud`).
#[derive(Debug, Clone)]
pub struct UpstreamDraft {
    /// Explicit user-provided alias (already normalized by the DTO layer).
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic (defaults to `true`).
    pub enabled: Option<bool>,
    /// Optional free-form tags.
    pub tags: Vec<String>,
    /// Server (endpoints + protocol) configuration.
    pub server: ServerConfig,
    /// Protocol used to connect to the upstream.
    pub protocol: UpstreamProtocol,
    /// Endpoint authentication.
    pub auth: AuthConfig,
    /// Request/response header policies.
    pub headers: HeadersConfig,
    /// Per-upstream rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// Per-upstream CORS behavior.
    pub cors: Option<CorsConfig>,
    /// Plugin references (built-in refs or custom plugin UUIDs), in binding
    /// order.
    pub plugin_refs: Vec<String>,
    /// Plugin sharing mode.
    pub plugins_sharing: SharingMode,
}

/// Domain "draft" for creating/replacing a route (see
/// [`UpstreamDraft`] for the layering rationale).
#[derive(Debug, Clone)]
pub struct RouteDraft {
    /// The tenant's upstream this route forwards to.
    pub upstream_id: Uuid,
    /// Route match rule (`Http` or `Grpc`).
    pub match_: RouteMatch,
    /// Match priority (larger wins).
    pub priority: i32,
    /// Whether the route is live (defaults to `true`).
    pub enabled: bool,
    /// Optional free-form tags.
    pub tags: Vec<String>,
    /// Per-route rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// Per-route CORS behavior.
    pub cors: Option<CorsConfig>,
    /// Plugin references (built-in refs or custom plugin UUIDs), in binding
    /// order.
    pub plugin_refs: Vec<String>,
    /// Plugin sharing mode.
    pub plugins_sharing: SharingMode,
}

/// Control Plane domain service.  Every public method takes the subject
/// [`SecurityContext`] and authorizes against the DESIGN §3.2/§3.3 GTS
/// permissions before touching a repository.
pub struct ControlPlaneService {
    /// Gear configuration (e.g. `allow_http_upstream`).
    pub cfg: OagwConfig,
    /// Upstream repository.
    pub upstreams: Arc<dyn UpstreamRepository>,
    /// Route repository.
    pub routes: Arc<dyn RouteRepository>,
    /// Plugin repository (also the custom-plugin registry).
    pub plugins: Arc<dyn PluginRepository>,
    /// Tenant resolution (management surface acts on the subject tenant of
    /// the `SecurityContext` directly).
    pub tenant_resolver: Arc<dyn TenantResolverClient>,
    /// Authorization client backing the PEP.
    pub authz_resolver: Arc<dyn AuthZResolverClient>,
    /// Policy enforcer built over `authz_resolver` (one enforcer serves all
    /// GTS resource types).
    enforcer: PolicyEnforcer,
}

impl ControlPlaneService {
    /// Builds the service and its policy enforcer.
    #[must_use]
    pub fn new(
        cfg: OagwConfig,
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
        authz_resolver: Arc<dyn AuthZResolverClient>,
    ) -> Self {
        let enforcer = PolicyEnforcer::new(authz_resolver.clone());
        Self {
            cfg,
            upstreams,
            routes,
            plugins,
            tenant_resolver,
            authz_resolver,
            enforcer,
        }
    }

    /// Authorizes `action` on `resource` for the subject context and returns
    /// the granted access scope (algorithm
    /// `cpt-cf-oagw-algo-control-plane-api-authorize`).  The access request
    /// pins `owner_tenant_id` to the subject tenant and requires non-empty
    /// constraints so a misconfigured PDP cannot widen access.
    async fn authorize(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &str,
    ) -> Result<AccessScope, DomainError> {
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .require_constraints(true);
        self.enforcer
            .access_scope_with(ctx, resource, action, None, &request)
            .await
            .map_err(map_enforcer_err)
    }

    // ------------------------------------------------------------------
    // Alias resolution (algorithm `cpt-cf-oagw-algo-control-plane-api-derive-alias`)
    // ------------------------------------------------------------------

    /// Resolves the effective alias for an upstream endpoint pool:
    ///
    /// - hostname pools auto-derive (single hostname; longest common suffix
    ///   ≥ 2 labels validated against the PSL); a user-provided alias that
    ///   normalizes to the derived value is an idempotent no-op, any other
    ///   user alias is a 400;
    /// - IP-based and non-derivable pools require an explicit alias, else 400;
    /// - the resolved alias is pattern-validated (`validate_alias`).
    fn resolve_alias(
        &self,
        endpoints: &[Endpoint],
        user_alias: Option<&str>,
    ) -> Result<String, DomainError> {
        let requires_explicit = requires_explicit_alias(endpoints);
        let derived = compute_derived_alias(endpoints);
        let user = user_alias.map(normalize_alias);

        let alias = match (requires_explicit, user) {
            // Derivable pool, no user alias → auto-derive.
            (false, None) => match derived {
                Some(d) => d,
                None => {
                    return Err(DomainError::validation(
                        Some("alias"),
                        "endpoints do not share a derivable hostname suffix; provide an explicit alias",
                    ));
                }
            },
            // Derivable pool, matching user alias → idempotent no-op.
            (false, Some(u)) => match derived {
                Some(d) if u == d => d,
                Some(d) => {
                    return Err(DomainError::validation(
                        Some("alias"),
                        format!(
                            "hostname-based pools derive their alias automatically; \
                             the provided alias '{u}' does not match the derived alias '{d}'"
                        ),
                    ));
                }
                None => {
                    return Err(DomainError::validation(
                        Some("alias"),
                        "endpoints do not share a derivable hostname suffix; provide an explicit alias",
                    ));
                }
            },
            // Non-derivable pool: explicit alias required.
            (true, Some(u)) => u,
            (true, None) => {
                return Err(DomainError::validation(
                    Some("alias"),
                    "an explicit alias is required for this endpoint pool (IP-based or non-derivable)",
                ));
            }
        };

        validate_alias(&alias).map_err(|e| DomainError::validation(Some("alias"), e))?;
        Ok(alias)
    }

    /// Builds a new [`Upstream`] from a draft: SSRF-guards every endpoint
    /// (scheme allowlist + RFC 1123 validation, algorithm
    /// `cpt-cf-oagw-algo-control-plane-api-ssrf-guard`), resolves the alias,
    /// and applies unified default configuration.  Returns the plugin
    /// references and sharing mode alongside so the caller can resolve
    /// bindings once the alias uniqueness is established.
    fn build_upstream(
        &self,
        tenant_id: Uuid,
        draft: UpstreamDraft,
    ) -> Result<(Upstream, Vec<String>, SharingMode), DomainError> {
        let UpstreamDraft {
            alias,
            enabled,
            tags,
            server,
            protocol,
            auth,
            headers,
            rate_limit,
            cors,
            plugin_refs,
            plugins_sharing,
        } = draft;

        for ep in &server.endpoints {
            validate_endpoint_url(ep, self.cfg.allow_http_upstream)
                .map_err(|e| DomainError::validation(Some("server.endpoints"), e))?;
        }
        let alias = self.resolve_alias(&server.endpoints, alias.as_deref())?;

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            protocol,
            enabled: enabled.unwrap_or(true),
            server,
            auth,
            headers,
            rate_limit,
            cors,
            plugins: PluginsConfig::default(),
            tags,
            created_at: None,
            updated_at: None,
        };
        Ok((upstream, plugin_refs, plugins_sharing))
    }

    /// Builds a [`Route`] from a draft, rejecting gRPC match rules (reserved
    /// for Phase 3, DoD `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).
    /// Returns the plugin references and sharing mode alongside.
    fn build_route(
        &self,
        tenant_id: Uuid,
        draft: RouteDraft,
    ) -> Result<(Route, Vec<String>, SharingMode), DomainError> {
        let RouteDraft {
            upstream_id,
            match_,
            priority,
            enabled,
            tags,
            rate_limit,
            cors,
            plugin_refs,
            plugins_sharing,
        } = draft;

        if matches!(match_, RouteMatch::Grpc(_)) {
            return Err(DomainError::validation(
                Some("match"),
                "gRPC route matching is reserved (Phase 3) and not served by the management surface",
            ));
        }
        match &match_ {
            RouteMatch::Http(m) if m.path_prefix.trim().is_empty() => {
                return Err(DomainError::validation(
                    Some("match.path"),
                    "an HTTP route requires a non-empty path prefix",
                ));
            }
            RouteMatch::Http(m) if m.methods.is_empty() => {
                return Err(DomainError::validation(
                    Some("match.methods"),
                    "an HTTP route requires at least one method",
                ));
            }
            _ => {}
        }

        let match_type = match &match_ {
            RouteMatch::Http(_) => MatchType::Http,
            RouteMatch::Grpc(_) => MatchType::Grpc,
        };
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            match_type,
            priority,
            enabled,
            match_,
            rate_limit,
            cors,
            plugins: PluginsConfig::default(),
            tags,
            created_at: None,
            updated_at: None,
        };
        Ok((route, plugin_refs, plugins_sharing))
    }

    /// Resolves plugin references into a [`PluginsConfig`] with contiguous
    /// positions (algorithm
    /// `cpt-cf-oagw-algo-control-plane-api-resolve-plugin-refs`).  A bare
    /// `{uuid}` reference must name an existing tenant-created plugin, else a
    /// 400 is returned up front.
    async fn resolve_plugin_bindings(
        &self,
        tenant_id: Uuid,
        refs: Vec<String>,
        sharing: SharingMode,
    ) -> Result<PluginsConfig, DomainError> {
        let mut items = Vec::with_capacity(refs.len());
        for (index, plugin_ref) in refs.into_iter().enumerate() {
            let plugin_uuid = if Uuid::parse_str(&plugin_ref).is_ok() {
                if self
                    .plugins
                    .catalog_entry(tenant_id, &plugin_ref)
                    .await
                    .is_none()
                {
                    return Err(DomainError::validation(
                        Some("plugins.items"),
                        format!(
                            "plugin reference '{plugin_ref}' does not name a plugin in this tenant"
                        ),
                    ));
                }
                Uuid::parse_str(&plugin_ref).ok()
            } else {
                None
            };
            items.push(PluginBinding {
                position: index as u32,
                plugin_ref,
                plugin_uuid,
                config: serde_json::Value::Null,
            });
        }
        Ok(PluginsConfig { sharing, items })
    }

    /// Returns the GTS resource type governing a plugin of `plugin_type`
    /// (`auth_plugin`, `guard_plugin`, or `transform_plugin`).
    fn plugin_resource(plugin_type: PluginType) -> ResourceType {
        let name = match plugin_type {
            PluginType::Auth => "gts.cf.core.oagw.auth_plugin.v1~",
            PluginType::Guard => "gts.cf.core.oagw.guard_plugin.v1~",
            PluginType::Transform => "gts.cf.core.oagw.transform_plugin.v1~",
        };
        ResourceType::new(name, &[pep_properties::OWNER_TENANT_ID])
    }

    // ------------------------------------------------------------------
    // Upstream CRUD (flow `cpt-cf-oagw-flow-control-plane-api-upstream-crud`)
    // ------------------------------------------------------------------

    /// Creates an upstream after authorizing `create` on `upstream.v1~`.
    /// Duplicate `(tenant_id, alias)` surfaces as 409 `upstream.alias_conflict`
    /// (algorithm `cpt-cf-oagw-algo-control-plane-api-validate-alias`).
    ///
    /// # Errors
    ///
    /// - `DomainError::AccessDenied` (403) / `DomainError::ServiceUnavailable` (503)
    ///   on authorization failure;
    /// - `DomainError::validation` (400) on alias/endpoint/plugin-ref issues;
    /// - `DomainError::AliasConflict` (409) on duplicate alias.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        draft: UpstreamDraft,
    ) -> Result<Upstream, DomainError> {
        let _scope = self
            .authorize(ctx, &authz::UPSTREAM, authz::actions::CREATE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let (mut upstream, plugin_refs, plugins_sharing) = self.build_upstream(tenant_id, draft)?;

        if let Some(existing) = self
            .upstreams
            .find_by_alias(tenant_id, &upstream.alias)
            .await
        {
            return Err(DomainError::AliasConflict {
                alias: existing.alias,
            });
        }
        upstream.plugins = self
            .resolve_plugin_bindings(tenant_id, plugin_refs, plugins_sharing)
            .await?;
        self.upstreams.create(tenant_id, upstream).await
    }

    /// Replaces an existing upstream (authorizes `override` on `upstream.v1~`).
    /// The alias is immutable: a replacement whose resolved alias differs from
    /// the stored one is a 400; ancestors are invisible (tenant-scoped), so a
    /// missing row yields `Ok(None)`.
    ///
    /// # Errors
    ///
    /// As [`Self::create_upstream`], plus `DomainError::validation` (400) when
    /// the replacement attempts to change the alias.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        draft: UpstreamDraft,
    ) -> Result<Option<Upstream>, DomainError> {
        let _scope = self
            .authorize(ctx, &authz::UPSTREAM, authz::actions::OVERRIDE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let Some(existing) = self.upstreams.get(tenant_id, id).await else {
            return Ok(None);
        };
        let (mut upstream, plugin_refs, plugins_sharing) = self.build_upstream(tenant_id, draft)?;
        upstream.id = id;

        if upstream.alias != existing.alias {
            return Err(DomainError::validation(
                Some("alias"),
                format!(
                    "alias is immutable once set; the upstream is addressed as '{}'",
                    existing.alias
                ),
            ));
        }
        upstream.plugins = self
            .resolve_plugin_bindings(tenant_id, plugin_refs, plugins_sharing)
            .await?;
        // `(tenant_id, alias)` uniqueness is preserved by construction (alias
        // unchanged), but the repository still re-runs the guard.
        self.upstreams.update(tenant_id, upstream).await
    }

    /// Reads one upstream (authorizes `read` on `upstream.v1~`; tenant-scoped).
    /// `Ok(None)` when the upstream is not visible in the subject tenant.
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Option<Upstream>, DomainError> {
        let scope = self
            .authorize(ctx, &authz::UPSTREAM, authz::actions::READ)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let Some(upstream) = self.upstreams.get(tenant_id, id).await else {
            return Ok(None);
        };
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, upstream.tenant_id) {
            return Ok(None);
        }
        Ok(Some(upstream))
    }

    /// Lists the subject tenant's upstreams (authorizes `read`).
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Upstream>, DomainError> {
        let _scope = self
            .authorize(ctx, &authz::UPSTREAM, authz::actions::READ)
            .await?;
        Ok(self.upstreams.list(ctx.subject_tenant_id()).await)
    }

    /// Deletes one upstream (authorizes `delete`).  `Ok(false)` means the
    /// upstream was not found/not visible in the tenant.
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let scope = self
            .authorize(ctx, &authz::UPSTREAM, authz::actions::DELETE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let Some(upstream) = self.upstreams.get(tenant_id, id).await else {
            return Ok(false);
        };
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, upstream.tenant_id) {
            return Ok(false);
        }
        self.upstreams.delete(tenant_id, id).await
    }

    // ------------------------------------------------------------------
    // Route CRUD (flow `cpt-cf-oagw-flow-control-plane-api-route-crud`)
    // ------------------------------------------------------------------

    /// Creates a route after authorizing `create` on `route.v1~`.  The
    /// referenced upstream must exist in the tenant (400 otherwise); two live
    /// routes on the same upstream sharing a method and an equal
    /// `(path_prefix, priority)` collide as 409 `route.conflict` (algorithm
    /// `cpt-cf-oagw-algo-control-plane-api-validate-match-rule`).
    ///
    /// # Errors
    ///
    /// 400/403/409/503 as documented above.
    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        draft: RouteDraft,
    ) -> Result<Route, DomainError> {
        let _scope = self
            .authorize(ctx, &authz::ROUTE, authz::actions::CREATE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        if self
            .upstreams
            .get(tenant_id, draft.upstream_id)
            .await
            .is_none()
        {
            return Err(DomainError::validation(
                Some("upstream_id"),
                format!(
                    "referenced upstream '{}' does not exist in this tenant",
                    draft.upstream_id
                ),
            ));
        }
        let (mut route, plugin_refs, plugins_sharing) = self.build_route(tenant_id, draft)?;
        route.plugins = self
            .resolve_plugin_bindings(tenant_id, plugin_refs, plugins_sharing)
            .await?;
        if route.enabled {
            self.check_route_collision(tenant_id, &route).await?;
        }
        self.routes.create(tenant_id, route).await
    }

    /// Replaces a route (authorizes `override`).  `upstream_id` is immutable:
    /// a replacement targeting a different upstream is a 400.  `Ok(None)`
    /// means the route is not visible in the tenant.
    ///
    /// # Errors
    ///
    /// As [`Self::create_route`], plus 400 on `upstream_id` change.
    pub async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        draft: RouteDraft,
    ) -> Result<Option<Route>, DomainError> {
        let _scope = self
            .authorize(ctx, &authz::ROUTE, authz::actions::OVERRIDE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let Some(existing) = self.routes.get(tenant_id, id).await else {
            return Ok(None);
        };
        if draft.upstream_id != existing.upstream_id {
            return Err(DomainError::validation(
                Some("upstream_id"),
                "upstream_id is immutable on a route; delete and recreate instead",
            ));
        }
        let (mut route, plugin_refs, plugins_sharing) = self.build_route(tenant_id, draft)?;
        route.id = id;
        route.plugins = self
            .resolve_plugin_bindings(tenant_id, plugin_refs, plugins_sharing)
            .await?;
        if route.enabled {
            self.check_route_collision(tenant_id, &route).await?;
        }
        self.routes.update(tenant_id, route).await
    }

    /// Reads one route (authorizes `read`; tenant-scoped).  `Ok(None)` when
    /// the route is not visible in the subject tenant.
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn get_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Option<Route>, DomainError> {
        let scope = self
            .authorize(ctx, &authz::ROUTE, authz::actions::READ)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let Some(route) = self.routes.get(tenant_id, id).await else {
            return Ok(None);
        };
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, route.tenant_id) {
            return Ok(None);
        }
        Ok(Some(route))
    }

    /// Lists the subject tenant's routes (authorizes `read`).
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<Route>, DomainError> {
        let _scope = self
            .authorize(ctx, &authz::ROUTE, authz::actions::READ)
            .await?;
        Ok(self.routes.list(ctx.subject_tenant_id()).await)
    }

    /// Deletes one route (authorizes `delete`).  `Ok(false)` when not visible.
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<bool, DomainError> {
        let scope = self
            .authorize(ctx, &authz::ROUTE, authz::actions::DELETE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        let Some(route) = self.routes.get(tenant_id, id).await else {
            return Ok(false);
        };
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, route.tenant_id) {
            return Ok(false);
        }
        self.routes.delete(tenant_id, id).await
    }

    /// Route collision pre-check: two *enabled* routes on the same upstream
    /// sharing a method and an equal `(path_prefix, priority)` conflict
    /// (algorithm `cpt-cf-oagw-algo-control-plane-api-validate-match-rule`).
    /// Replaces exclude their own row (`candidate.id`).
    async fn check_route_collision(
        &self,
        tenant_id: Uuid,
        candidate: &Route,
    ) -> Result<(), DomainError> {
        let cand_http =
            http_match_of(candidate).map_err(|e| DomainError::validation(Some("match"), e))?;
        for other in self
            .routes
            .list_by_upstream(tenant_id, candidate.upstream_id)
            .await
        {
            if other.id == candidate.id || !other.enabled {
                continue;
            }
            let Ok(other_http) = http_match_of(&other) else {
                continue;
            };
            let shares_method = cand_http
                .methods
                .iter()
                .any(|m| other_http.methods.contains(m));
            if shares_method
                && other_http.path_prefix == cand_http.path_prefix
                && other.priority == candidate.priority
            {
                return Err(DomainError::RouteConflict {
                    detail: format!(
                        "route conflicts with route '{}' on upstream '{}' \
                         (same path_prefix '{}' and priority {})",
                        other.id, other.upstream_id, other_http.path_prefix, other.priority
                    ),
                });
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugin CRUD (flow `cpt-cf-oagw-flow-control-plane-api-plugin-crud`)
    // ------------------------------------------------------------------

    /// Creates a custom plugin (authorizes `create` on the type-specific GTS
    /// resource).  A `(tenant_id, name)` duplicate is 409 `plugin.conflict`.
    ///
    /// # Errors
    ///
    /// 400/403/409/503 as documented.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        plugin_type: PluginType,
        name: String,
        config_schema: serde_json::Value,
        source_code: String,
    ) -> Result<Plugin, DomainError> {
        let resource = Self::plugin_resource(plugin_type);
        let _scope = self
            .authorize(ctx, &resource, authz::actions::CREATE)
            .await?;
        let tenant_id = ctx.subject_tenant_id();
        if self.plugins.find_by_name(tenant_id, &name).await.is_some() {
            return Err(DomainError::PluginConflict { name });
        }
        let plugin = Plugin::new(tenant_id, plugin_type, name, config_schema, source_code);
        self.plugins.create(tenant_id, plugin).await
    }

    /// Reads one custom plugin (authorizes `read` on the type-specific
    /// resource; tenant-scoped).  `Ok(None)` when not visible.
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn get_plugin(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Option<Plugin>, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        let Some(plugin) = self.plugins.get(tenant_id, id).await else {
            return Ok(None);
        };
        let resource = Self::plugin_resource(plugin.plugin_type);
        let scope = self.authorize(ctx, &resource, authz::actions::READ).await?;
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, plugin.tenant_id) {
            return Ok(None);
        }
        Ok(Some(plugin))
    }

    /// Lists the subject tenant's custom plugins (authorizes `read` per
    /// plugin type; drops any plugin outside the granted scope).
    ///
    /// A caller lacking `read` on one plugin type does not abort the list: the
    /// denied plugins are skipped and the rest are returned (drop semantics).
    /// Only a hard authorization *evaluation* failure (503
    /// `service.unavailable`) propagates.
    ///
    /// # Errors
    ///
    /// 503 on authorization evaluation failure only; per-type denials are
    /// skipped, not propagated.
    pub async fn list_plugins(&self, ctx: &SecurityContext) -> Result<Vec<Plugin>, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        let mut all = self.plugins.list(tenant_id).await;
        let mut visible = Vec::with_capacity(all.len());
        for plugin in all.drain(..) {
            let resource = Self::plugin_resource(plugin.plugin_type);
            let scope = match self.authorize(ctx, &resource, authz::actions::READ).await {
                Ok(scope) => scope,
                // A plain denial for this plugin type: drop the plugin from
                // the list and continue (matches the documented drop
                // semantics; do not fail the whole list).
                Err(DomainError::AccessDenied { .. }) => continue,
                Err(err) => return Err(err),
            };
            if scope.contains_uuid(pep_properties::OWNER_TENANT_ID, plugin.tenant_id) {
                visible.push(plugin);
            }
        }
        Ok(visible)
    }

    /// Returns the source code of a custom plugin (authorizes `read`).
    /// `Ok(None)` when the plugin is not visible in the subject tenant.
    ///
    /// # Errors
    ///
    /// 403/503 on authorization failure only.
    pub async fn plugin_source(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        let Some(plugin) = self.plugins.get(tenant_id, id).await else {
            return Ok(None);
        };
        let resource = Self::plugin_resource(plugin.plugin_type);
        let scope = self.authorize(ctx, &resource, authz::actions::READ).await?;
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, plugin.tenant_id) {
            return Ok(None);
        }
        Ok(self.plugins.source(tenant_id, id).await)
    }

    /// Deletes a custom plugin (authorizes `delete` on the type-specific
    /// resource).  A plugin still bound by upstreams/routes is refused with
    /// 409 `plugin.in_use` carrying the `referenced_by` shape.
    ///
    /// # Errors
    ///
    /// 403/409/503 as documented.
    pub async fn delete_plugin(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<bool, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        let Some(plugin) = self.plugins.get(tenant_id, id).await else {
            return Ok(false);
        };
        let resource = Self::plugin_resource(plugin.plugin_type);
        let scope = self
            .authorize(ctx, &resource, authz::actions::DELETE)
            .await?;
        if !scope.contains_uuid(pep_properties::OWNER_TENANT_ID, plugin.tenant_id) {
            return Ok(false);
        }
        self.plugins.delete(tenant_id, id).await
    }
}
