//! Control-plane service: CRUD for upstreams, routes and custom plugins.
//!
//! All resources are strictly scoped to the calling tenant (ancestor
//! resources are invisible through the management API).

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::model::{
    CorsConfig, MatchConfig, Plugin, PluginKind, PluginsConfig, Protocol, RateLimitConfig,
    Route, ServerConfig, SharingMode, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::hierarchy::TenantHierarchy;

// Authorization action names (DESIGN §3.3).
const ACTION_CREATE: &str = "create";
const ACTION_OVERRIDE: &str = "override";
const ACTION_READ: &str = "read";
const ACTION_DELETE: &str = "delete";

/// Create/update input for an upstream (id/tenant_id are server-assigned).
#[derive(Debug, Clone, Default)]
pub struct UpstreamInput {
    pub enabled: bool,
    pub alias: Option<String>,
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    pub auth: crate::domain::model::AuthConfig,
    pub headers: crate::domain::model::HeadersConfig,
    pub plugins: PluginsConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
}

impl UpstreamInput {
    fn validate(&self) -> Result<(), DomainError> {
        self.server.validate(self.protocol)?;
        if let Some(cors) = &self.cors {
            cors.validate()?;
        }
        if let Some(rl) = &self.rate_limit {
            if rl.sustained.rate == 0 {
                return Err(DomainError::validation(
                    "rate_limit.sustained.rate must be >= 1",
                ));
            }
        }
        Ok(())
    }
}

/// Create/update input for a route.
#[derive(Debug, Clone, Default)]
pub struct RouteInput {
    pub enabled: bool,
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    pub priority: i32,
    pub match_: MatchConfig,
    pub plugins: PluginsConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
}

impl RouteInput {
    fn validate(&self) -> Result<(), DomainError> {
        self.match_.validate()?;
        if let Some(cors) = &self.cors {
            cors.validate()?;
        }
        Ok(())
    }
}

/// Create input for a custom plugin.
#[derive(Debug, Clone)]
pub struct PluginInput {
    pub name: String,
    pub description: Option<String>,
    pub kind: PluginKind,
    pub config_schema: serde_json::Value,
    pub source_code: String,
}

/// Control-plane service contract.
#[async_trait]
pub trait ControlPlaneService: Send + Sync {
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamInput,
    ) -> Result<Upstream, DomainError>;

    async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, DomainError>;

    async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Upstream>, DomainError>;

    async fn update_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, DomainError>;

    async fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), DomainError>;

    async fn set_upstream_enabled(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, DomainError>;

    async fn create_route(
        &self,
        ctx: &SecurityContext,
        input: RouteInput,
    ) -> Result<Route, DomainError>;

    async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<Route, DomainError>;

    async fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<Route>, DomainError>;

    async fn update_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, DomainError>;

    async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError>;

    async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        input: PluginInput,
    ) -> Result<Plugin, DomainError>;

    async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<Plugin, DomainError>;

    async fn list_plugins(&self, ctx: &SecurityContext) -> Result<Vec<Plugin>, DomainError>;

    async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError>;

    async fn get_plugin_source(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<String, DomainError>;
}

/// Concrete in-memory control-plane implementation.
pub struct ControlPlaneServiceImpl {
    pub upstreams: Arc<dyn UpstreamRepository>,
    pub routes: Arc<dyn RouteRepository>,
    pub plugins: Arc<dyn PluginRepository>,
    pub hierarchy: Arc<dyn TenantHierarchy>,
    pub authz: PolicyEnforcer,
}

impl ControlPlaneServiceImpl {
    /// Build a new service.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        authz: PolicyEnforcer,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            hierarchy,
            authz,
        }
    }

    async fn authorize_resource(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &str,
    ) -> Result<(), DomainError> {
        let req = AccessRequest::default().require_constraints(false);
        match self
            .authz
            .access_scope_with(ctx, resource, action, None, &req)
            .await
        {
            Ok(_) => Ok(()),
            Err(EnforcerError::Denied { .. }) => Err(DomainError::AccessDenied(format!(
                "{}:{} denied",
                resource.name(),
                action
            ))),
            Err(e) => Err(DomainError::Internal(format!("authz evaluation failed: {e}"))),
        }
    }

    /// Enforce ancestor binding constraints for a new/updated alias.
    async fn check_ancestor_binding(
        &self,
        tenant: Uuid,
        alias: &str,
    ) -> Result<(), DomainError> {
        let chain = self.hierarchy.tenant_chain(tenant).await?;
        // chain = [self, parent, ..., root]; ancestors are the tail.
        for anc in chain.iter().skip(1) {
            if let Some(existing) = self.upstreams.get_by_alias(*anc, alias) {
                match existing.auth.sharing {
                    SharingMode::Enforce => {
                        return Err(DomainError::AliasConflict(alias.to_owned(), SharingMode::Enforce));
                    }
                    SharingMode::Private => {
                        return Err(DomainError::AliasConflict(alias.to_owned(), SharingMode::Private));
                    }
                    SharingMode::Inherit => {
                        // Binding-style flow: allowed with the bind permission.
                        // The caller (handlers) enforces permission; here we
                        // accept the binding.
                        let _ = existing;
                    }
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ControlPlaneService for ControlPlaneServiceImpl {
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamInput,
    ) -> Result<Upstream, DomainError> {
        input.validate()?;
        self.authorize_resource(ctx, &crate::gts::UPSTREAM_RESOURCE, ACTION_CREATE)
            .await?;

        let tenant = ctx.subject_tenant_id();
        let alias = self
            .hierarchy
            .tenant_chain(tenant)
            .await
            .map_err(|_| {
                DomainError::Internal("cannot resolve tenant hierarchy for alias check".to_owned())
            })
            .and_then(|_| {
                alias::enforce_alias_update(
                    |a| self.upstreams.alias_taken(tenant, a),
                    None,
                    false,
                    input.alias.as_deref(),
                    input.protocol,
                    &input.server,
                )
            })?;

        self.check_ancestor_binding(tenant, &alias).await?;

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            enabled: input.enabled,
            alias: Some(alias),
            tags: input.tags.clone(),
            server: input.server.clone(),
            protocol: input.protocol,
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            bound: false,
        };
        self.upstreams.insert(upstream.clone())?;
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, DomainError> {
        self.authorize_resource(ctx, &crate::gts::UPSTREAM_RESOURCE, ACTION_READ)
            .await?;
        self.upstreams
            .get_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| DomainError::not_found("upstream", id))
    }

    async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Upstream>, DomainError> {
        self.authorize_resource(ctx, &crate::gts::UPSTREAM_RESOURCE, ACTION_READ)
            .await?;
        Ok(self.upstreams.list(ctx.subject_tenant_id()))
    }

    async fn update_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, DomainError> {
        input.validate()?;
        self.authorize_resource(ctx, &crate::gts::UPSTREAM_RESOURCE, ACTION_OVERRIDE)
            .await?;

        let tenant = ctx.subject_tenant_id();
        let existing = self
            .upstreams
            .get_by_id(tenant, id)
            .ok_or_else(|| DomainError::not_found("upstream", id))?;

        // Recompute alias (immutable once set).
        let existing_derivable =
            alias::compute_derived_alias(existing.protocol, &existing.server).is_some();
        let alias = alias::enforce_alias_update(
            |a| {
                // The alias may belong to the row being replaced.
                self.upstreams
                    .alias_taken(tenant, a)
                    && self
                        .upstreams
                        .get_by_alias(tenant, a)
                        .is_some_and(|o| o.id != id)
            },
            existing.alias.as_deref(),
            existing_derivable,
            input.alias.as_deref(),
            input.protocol,
            &input.server,
        )?;

        if existing.alias.as_deref() != Some(alias.as_str()) {
            self.check_ancestor_binding(tenant, &alias).await?;
        }

        let updated = Upstream {
            id,
            tenant_id: tenant,
            enabled: input.enabled,
            alias: Some(alias),
            tags: input.tags.clone(),
            server: input.server.clone(),
            protocol: input.protocol,
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            bound: existing.bound,
        };
        self.upstreams.replace(tenant, updated.clone())?;
        Ok(updated)
    }

    async fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), DomainError> {
        self.authorize_resource(ctx, &crate::gts::UPSTREAM_RESOURCE, ACTION_DELETE)
            .await?;
        let tenant = ctx.subject_tenant_id();
        // Cascade: remove tenant routes bound to this upstream.
        let routes = self.routes.list_for_upstream(tenant, id);
        for r in routes {
            self.routes.delete(tenant, r.id)?;
        }
        self.upstreams.delete(tenant, id)?;
        Ok(())
    }

    async fn set_upstream_enabled(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, DomainError> {
        self.authorize_resource(ctx, &crate::gts::UPSTREAM_RESOURCE, ACTION_OVERRIDE)
            .await?;
        let tenant = ctx.subject_tenant_id();
        let mut u = self
            .upstreams
            .get_by_id(tenant, id)
            .ok_or_else(|| DomainError::not_found("upstream", id))?;
        u.enabled = enabled;
        self.upstreams.replace(tenant, u.clone())?;
        Ok(u)
    }

    async fn create_route(
        &self,
        ctx: &SecurityContext,
        input: RouteInput,
    ) -> Result<Route, DomainError> {
        input.validate()?;
        self.authorize_resource(ctx, &crate::gts::ROUTE_RESOURCE, ACTION_CREATE)
            .await?;

        let tenant = ctx.subject_tenant_id();
        // upstream_id must belong to the calling tenant.
        if self.upstreams.get_by_id(tenant, input.upstream_id).is_none() {
            return Err(DomainError::validation(format!(
                "upstream_id '{}' does not belong to this tenant",
                input.upstream_id
            )));
        }
        self.check_route_uniqueness(tenant, input.upstream_id, None, &input)?;

        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id: input.upstream_id,
            enabled: input.enabled,
            tags: input.tags.clone(),
            priority: input.priority,
            match_: input.match_.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
        };
        self.routes.insert(route.clone())?;
        Ok(route)
    }

    async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<Route, DomainError> {
        self.authorize_resource(ctx, &crate::gts::ROUTE_RESOURCE, ACTION_READ)
            .await?;
        self.routes
            .get_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| DomainError::not_found("route", id))
    }

    async fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<Route>, DomainError> {
        self.authorize_resource(ctx, &crate::gts::ROUTE_RESOURCE, ACTION_READ)
            .await?;
        Ok(self.routes.list(ctx.subject_tenant_id()))
    }

    async fn update_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, DomainError> {
        input.validate()?;
        self.authorize_resource(ctx, &crate::gts::ROUTE_RESOURCE, ACTION_OVERRIDE)
            .await?;

        let tenant = ctx.subject_tenant_id();
        let existing = self
            .routes
            .get_by_id(tenant, id)
            .ok_or_else(|| DomainError::not_found("route", id))?;

        // upstream_id is immutable; route always stays bound to its upstream.
        if self.upstreams.get_by_id(tenant, input.upstream_id).is_none() {
            return Err(DomainError::validation(format!(
                "upstream_id '{}' does not belong to this tenant",
                input.upstream_id
            )));
        }
        self.check_route_uniqueness(tenant, input.upstream_id, Some(id), &input)?;

        let updated = Route {
            id,
            tenant_id: tenant,
            upstream_id: existing.upstream_id,
            enabled: input.enabled,
            tags: input.tags.clone(),
            priority: input.priority,
            match_: input.match_.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
        };
        self.routes.replace(tenant, updated.clone())?;
        Ok(updated)
    }

    async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        self.authorize_resource(ctx, &crate::gts::ROUTE_RESOURCE, ACTION_DELETE)
            .await?;
        self.routes.delete(ctx.subject_tenant_id(), id)?;
        Ok(())
    }

    async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        input: PluginInput,
    ) -> Result<Plugin, DomainError> {
        self.authorize_resource(
            ctx,
            &crate::gts::plugin_resource(input.kind),
            ACTION_CREATE,
        )
        .await?;

        let tenant = ctx.subject_tenant_id();
        if input.name.trim().is_empty() {
            return Err(DomainError::validation("plugin name must not be empty"));
        }
        if self.plugins.get_by_name(tenant, &input.name).is_some() {
            return Err(DomainError::AliasViolation(format!(
                "a plugin named '{}' already exists in this tenant",
                input.name
            )));
        }
        if input.source_code.trim().is_empty() {
            return Err(DomainError::validation("plugin source_code must not be empty"));
        }

        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            name: input.name.trim().to_owned(),
            description: input.description.clone(),
            kind: input.kind,
            config_schema: input.config_schema.clone(),
            source_code: input.source_code.clone(),
            gc_eligible_at: None,
        };
        self.plugins.insert(plugin.clone())?;
        Ok(plugin)
    }

    async fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<Plugin, DomainError> {
        self.authorize_resource(
            ctx,
            &crate::gts::plugin_resource(PluginKind::Auth),
            ACTION_READ,
        )
        .await?;
        self.plugins
            .get_by_id(ctx.subject_tenant_id(), id)
            .ok_or_else(|| DomainError::not_found("plugin", id))
    }

    async fn list_plugins(&self, ctx: &SecurityContext) -> Result<Vec<Plugin>, DomainError> {
        self.authorize_resource(
            ctx,
            &crate::gts::plugin_resource(PluginKind::Auth),
            ACTION_READ,
        )
        .await?;
        Ok(self.plugins.list(ctx.subject_tenant_id()))
    }

    async fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), DomainError> {
        let tenant = ctx.subject_tenant_id();
        let plugin = self
            .plugins
            .get_by_id(tenant, id)
            .ok_or_else(|| DomainError::not_found("plugin", id))?;

        // Reject deletion while the plugin is referenced by an upstream or
        // route (DESIGN: DELETE returns 409 PluginInUse when referenced).
        for u in self.upstreams.list(tenant) {
            if u.plugins.items.iter().any(|b| b.plugin_uuid == Some(id))
                || (u.auth.auth_type == plugin.gts_id())
            {
                return Err(DomainError::PluginInUse(plugin.name.clone()));
            }
        }
        for r in self.routes.list(tenant) {
            if r.plugins.items.iter().any(|b| b.plugin_uuid == Some(id)) {
                return Err(DomainError::PluginInUse(plugin.name.clone()));
            }
        }

        self.plugins.delete(tenant, id)?;
        Ok(())
    }

    async fn get_plugin_source(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<String, DomainError> {
        let plugin = self.get_plugin(ctx, id).await?;
        Ok(plugin.source_code)
    }
}

impl ControlPlaneServiceImpl {
    /// Route match determinism: no two enabled routes under the same upstream
    /// may share `(path_prefix, priority)` for the same method.
    fn check_route_uniqueness(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
        exclude: Option<Uuid>,
        input: &RouteInput,
    ) -> Result<(), DomainError> {
        let Some(http) = &input.match_.http else {
            return Ok(());
        };
        for existing in self.routes.list_for_upstream(tenant, upstream_id) {
            if exclude == Some(existing.id) {
                continue;
            }
            if !existing.enabled {
                continue;
            }
            if let Some(e_http) = existing.match_.http.as_ref()
                && e_http.path == http.path
                && existing.priority == input.priority
            {
                let overlap = e_http.methods.is_empty()
                    || http.methods.is_empty()
                    || http
                        .methods
                        .iter()
                        .any(|m| e_http.methods.iter().any(|em| em.eq_ignore_ascii_case(m)));
                if overlap {
                    return Err(DomainError::RouteConflict(format!(
                        "a route with path '{}' and priority {} already exists for this upstream",
                        http.path, input.priority
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{AuthConfig, Endpoint, HttpMatch};
    use crate::infra::storage::memory::{
        MemoryPluginRepository, MemoryRouteRepository, MemoryUpstreamRepository,
    };
    use crate::infra::storage::tenant_hierarchy::MemoryHierarchy;

    struct AllowAllAuthZ;

    #[async_trait]
    impl authz_resolver_sdk::AuthZResolverClient for AllowAllAuthZ {
        async fn evaluate(
            &self,
            _request: authz_resolver_sdk::EvaluationRequest,
        ) -> Result<authz_resolver_sdk::EvaluationResponse, authz_resolver_sdk::AuthZResolverError>
        {
            Ok(authz_resolver_sdk::EvaluationResponse {
                decision: true,
                context: authz_resolver_sdk::EvaluationResponseContext::default(),
            })
        }
    }

    fn ctx(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("valid ctx")
    }

    struct Harness {
        svc: ControlPlaneServiceImpl,
        tenant: Uuid,
    }

    fn harness() -> Harness {
        let tenant = Uuid::new_v4();
        let svc = ControlPlaneServiceImpl::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            Arc::new(MemoryHierarchy::default()),
            PolicyEnforcer::new(Arc::new(AllowAllAuthZ)),
        );
        Harness { svc, tenant }
    }

    fn hostname_input(host: &str, port: u16) -> UpstreamInput {
        UpstreamInput {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".into(),
                    host: host.into(),
                    port,
                }],
            },
            protocol: Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn ip_input(alias: Option<&str>, host: &str) -> UpstreamInput {
        UpstreamInput {
            enabled: true,
            alias: alias.map(str::to_owned),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "http".into(),
                    host: host.into(),
                    port: 80,
                }],
            },
            protocol: Protocol::Http,
            auth: Default::default(),
            headers: Default::default(),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn http_route(upstream_id: Uuid, path: &str, priority: i32) -> RouteInput {
        RouteInput {
            enabled: true,
            tags: Vec::new(),
            upstream_id,
            priority,
            match_: MatchConfig {
                http: Some(HttpMatch {
                    methods: Vec::new(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[tokio::test]
    async fn create_derives_alias_from_hostname() {
        let h = harness();
        let created = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        assert_eq!(created.alias.as_deref(), Some("api.openai.com"));
        assert_eq!(created.tenant_id, h.tenant);
    }

    #[tokio::test]
    async fn create_rejects_user_alias_differing_from_derived() {
        let h = harness();
        let mut input = hostname_input("api.openai.com", 443);
        input.alias = Some("my-alias".into());
        let err = h.svc.create_upstream(&ctx(h.tenant), input).await.unwrap_err();
        assert!(err.to_string().contains("auto-derived"), "{err}");
    }

    #[tokio::test]
    async fn create_tolerates_exact_derived_alias() {
        let h = harness();
        let mut input = hostname_input("api.openai.com", 443);
        input.alias = Some("api.openai.com".into());
        let created = h.svc.create_upstream(&ctx(h.tenant), input).await.unwrap();
        assert_eq!(created.alias.as_deref(), Some("api.openai.com"));
    }

    #[tokio::test]
    async fn create_ip_without_alias_rejected() {
        let h = harness();
        let err = h
            .svc
            .create_upstream(&ctx(h.tenant), ip_input(None, "10.0.1.1"))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AliasViolation(_)));
    }

    #[tokio::test]
    async fn create_ip_with_explicit_alias_ok() {
        let h = harness();
        let created = h
            .svc
            .create_upstream(&ctx(h.tenant), ip_input(Some("my-service"), "10.0.1.1"))
            .await
            .unwrap();
        assert_eq!(created.alias.as_deref(), Some("my-service"));
    }

    #[tokio::test]
    async fn duplicate_alias_rejected() {
        let h = harness();
        h.svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        let err = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AliasViolation(_)));
    }

    #[tokio::test]
    async fn update_derivable_to_different_derivable_rejected() {
        let h = harness();
        let created = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.old.com", 443))
            .await
            .unwrap();
        let err = h
            .svc
            .update_upstream(&ctx(h.tenant), created.id, hostname_input("api.new.com", 443))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AliasViolation(_)));
        assert!(err.to_string().contains("immutable"), "{err}");
    }

    #[tokio::test]
    async fn update_with_unchanged_derived_alias_tolerated() {
        let h = harness();
        let created = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        let updated = h
            .svc
            .update_upstream(&ctx(h.tenant), created.id, hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        assert_eq!(updated.alias.as_deref(), Some("api.openai.com"));
    }

    #[tokio::test]
    async fn update_derivable_to_non_derivable_rejected() {
        let h = harness();
        let created = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        // Hostname → IP transition always rejected, even with an explicit alias.
        let err = h
            .svc
            .update_upstream(
                &ctx(h.tenant),
                created.id,
                ip_input(Some("api.openai.com"), "10.0.1.1"),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AliasViolation(_)));
    }

    #[tokio::test]
    async fn update_ip_retains_alias_when_omitted() {
        let h = harness();
        let created = h
            .svc
            .create_upstream(&ctx(h.tenant), ip_input(Some("my-service"), "10.0.1.1"))
            .await
            .unwrap();
        let updated = h
            .svc
            .update_upstream(&ctx(h.tenant), created.id, ip_input(None, "10.0.1.2"))
            .await
            .unwrap();
        assert_eq!(updated.alias.as_deref(), Some("my-service"));
    }

    #[tokio::test]
    async fn delete_upstream_cascades_routes() {
        let h = harness();
        let up = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        h.svc
            .create_route(&ctx(h.tenant), http_route(up.id, "/v1", 0))
            .await
            .unwrap();
        h.svc.delete_upstream(&ctx(h.tenant), up.id).await.unwrap();
        assert!(h.svc.routes.list(h.tenant).is_empty());
        assert!(h.svc.upstreams.get_by_id(h.tenant, up.id).is_none());
    }

    #[tokio::test]
    async fn create_route_rejects_foreign_upstream() {
        let h = harness();
        let other = Uuid::new_v4();
        let foreign = h
            .svc
            .create_upstream(&ctx(other), hostname_input("api.other.com", 443))
            .await
            .unwrap();
        let err = h
            .svc
            .create_route(&ctx(h.tenant), http_route(foreign.id, "/v1", 0))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
        assert!(err.to_string().contains("does not belong"));
    }

    #[tokio::test]
    async fn route_uniqueness_conflict_rejected() {
        let h = harness();
        let up = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        h.svc
            .create_route(&ctx(h.tenant), http_route(up.id, "/v1", 0))
            .await
            .unwrap();
        let err = h
            .svc
            .create_route(&ctx(h.tenant), http_route(up.id, "/v1", 0))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::RouteConflict(_)));
        // A different priority is a distinct route.
        h.svc
            .create_route(&ctx(h.tenant), http_route(up.id, "/v1", 1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn plugin_crud_and_in_use_block() {
        let h = harness();
        let input = PluginInput {
            name: "my-guard".into(),
            description: None,
            kind: PluginKind::Guard,
            config_schema: serde_json::Value::Null,
            source_code: "def main(): pass".into(),
        };
        let plugin = h
            .svc
            .create_plugin(&ctx(h.tenant), input.clone())
            .await
            .unwrap();
        assert_eq!(
            h.svc.get_plugin(&ctx(h.tenant), plugin.id).await.unwrap().name,
            "my-guard"
        );
        assert_eq!(
            h.svc.get_plugin_source(&ctx(h.tenant), plugin.id).await.unwrap(),
            "def main(): pass"
        );

        // Duplicate name rejected.
        let dup = PluginInput {
            name: "My-Guard".into(),
            kind: PluginKind::Guard,
            ..input.clone()
        };
        assert!(h.svc.create_plugin(&ctx(h.tenant), dup).await.is_err());

        // Bind it to an upstream, then delete → PluginInUse.
        let up = h
            .svc
            .create_upstream(&ctx(h.tenant), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
        let mut bound = hostname_input("api.openai.com", 443);
        bound.plugins = PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![crate::domain::model::PluginBinding {
                plugin_ref: plugin.gts_id(),
                plugin_uuid: Some(plugin.id),
                position: Some(0),
                config: serde_json::Value::Null,
            }],
        };
        h.svc.update_upstream(&ctx(h.tenant), up.id, bound).await.unwrap();
        let err = h.svc.delete_plugin(&ctx(h.tenant), plugin.id).await.unwrap_err();
        assert!(matches!(err, DomainError::PluginInUse(_)));
    }

    #[tokio::test]
    async fn delete_plugin_when_not_referenced_ok() {
        let h = harness();
        let plugin = h
            .svc
            .create_plugin(
                &ctx(h.tenant),
                PluginInput {
                    name: "free".into(),
                    kind: PluginKind::Transform,
                    config_schema: serde_json::Value::Null,
                    source_code: "x".into(),
                    description: None,
                },
            )
            .await
            .unwrap();
        h.svc.delete_plugin(&ctx(h.tenant), plugin.id).await.unwrap();
        assert!(h.svc.plugins.get_by_id(h.tenant, plugin.id).is_none());
    }

    #[tokio::test]
    async fn enforced_ancestor_alias_blocks_child_binding() {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let hierarchy = MemoryHierarchy::default().add_edge(child, root);
        let svc = ControlPlaneServiceImpl::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            Arc::new(hierarchy),
            PolicyEnforcer::new(Arc::new(AllowAllAuthZ)),
        );

        // Ancestor (root) defines the alias under `enforce`.
        let mut enforced = hostname_input("api.openai.com", 443);
        enforced.auth = AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".into(),
            sharing: SharingMode::Enforce,
            config: serde_json::Value::Null,
        };
        svc.create_upstream(&ctx(root), enforced).await.unwrap();

        // Child defining the same alias is rejected (alias conflict).
        let err = svc
            .create_upstream(&ctx(child), hostname_input("api.openai.com", 443))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AliasConflict(_, SharingMode::Enforce)));
    }

    #[tokio::test]
    async fn inherit_ancestor_allows_child_binding() {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let hierarchy = MemoryHierarchy::default().add_edge(child, root);
        let svc = ControlPlaneServiceImpl::new(
            Arc::new(MemoryUpstreamRepository::default()),
            Arc::new(MemoryRouteRepository::default()),
            Arc::new(MemoryPluginRepository::default()),
            Arc::new(hierarchy),
            PolicyEnforcer::new(Arc::new(AllowAllAuthZ)),
        );
        let mut shared = hostname_input("api.openai.com", 443);
        shared.auth = AuthConfig {
            auth_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1".into(),
            sharing: SharingMode::Inherit,
            config: serde_json::Value::Null,
        };
        svc.create_upstream(&ctx(root), shared).await.unwrap();
        // Binding-style: allowed under an inherit ancestor.
        svc.create_upstream(&ctx(child), hostname_input("api.openai.com", 443))
            .await
            .unwrap();
    }
}

