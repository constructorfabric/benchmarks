//! Control plane service contract: management CRUD, alias routing
//! resolution, and effective-configuration merge for the data plane.

use async_trait::async_trait;
use uuid::Uuid;

use super::model::{
    CorsConfig, PluginBinding, PluginChainConfig, PluginDef, RateLimitConfig, Route, Upstream,
};
use super::wire::{PluginInput, RouteInput, UpstreamInput};
use crate::error::OagwError;

/// Caller identity derived from the security context.
#[derive(Debug, Clone)]
pub struct Caller {
    pub tenant_id: Uuid,
    pub subject_id: Uuid,
    pub scopes: Vec<String>,
}

impl Caller {
    /// Build a caller from an authenticated security context.
    #[must_use]
    pub fn from_security_context(ctx: &toolkit_security::SecurityContext) -> Self {
        Self {
            tenant_id: ctx.subject_tenant_id(),
            subject_id: ctx.subject_id(),
            scopes: ctx.token_scopes().to_vec(),
        }
    }

    /// Rebuild a `SecurityContext` for SDK calls (credstore, tenant
    /// resolver) that require one.
    #[must_use]
    pub fn security_context(&self) -> toolkit_security::SecurityContext {
        toolkit_security::context::SecurityContextBuilder::default()
            .subject_id(self.subject_id)
            .subject_tenant_id(self.tenant_id)
            .token_scopes(self.scopes.clone())
            .build()
            .unwrap_or_else(|_| toolkit_security::SecurityContext::anonymous())
    }

    /// Whether this caller holds `permission` (or unrestricted `*`).
    #[must_use]
    pub fn has_permission(&self, permission: &str) -> bool {
        crate::gts_helpers::has_permission(&self.scopes, permission)
    }
}

/// Pagination for list operations (OData `$top` / `$skip`).
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Page size (default 50, max 100).
    pub top: usize,
    /// Items to skip.
    pub skip: usize,
}

impl ListOptions {
    /// Clamp per DESIGN.md: default 50, max 100.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        if self.top == 0 {
            self.top = 50;
        }
        self.top = self.top.min(100);
        self
    }
}

/// Result of alias resolution: the routing target plus the ancestor chain
/// that constrains it.
#[derive(Debug, Clone)]
pub struct UpstreamResolution {
    /// The closest upstream matching the alias (routing target).
    pub upstream: Upstream,
    /// Ancestor upstreams (excluding the selected one) that also bind the
    /// alias — `(record, enforcement)` ordered root-first.
    pub ancestors: Vec<Upstream>,
}

/// Result of route matching: effective route plus merged plugin chain.
#[derive(Debug, Clone)]
pub struct RouteResolution {
    pub route: Route,
    /// Path suffix preserved from the proxy URL (when the route matched a
    /// prefix and `path_suffix_mode` is `append`).
    pub path_suffix: String,
    /// Query parameters the client may send (route `query_allowlist`).
    pub query_allowlist: Vec<String>,
}

/// The management CRUD + routing resolution service.
#[async_trait]
pub trait ControlPlaneService: Send + Sync {
    // -- upstreams ---------------------------------------------------------

    async fn create_upstream(
        &self,
        caller: &Caller,
        input: UpstreamInput,
    ) -> Result<Upstream, OagwError>;

    async fn get_upstream(&self, caller: &Caller, id: Uuid) -> Result<Upstream, OagwError>;

    async fn list_upstreams(
        &self,
        caller: &Caller,
        opts: &ListOptions,
    ) -> Result<(Vec<Upstream>, usize), OagwError>;

    async fn update_upstream(
        &self,
        caller: &Caller,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, OagwError>;

    async fn delete_upstream(&self, caller: &Caller, id: Uuid) -> Result<(), OagwError>;

    // -- routes ------------------------------------------------------------

    async fn create_route(&self, caller: &Caller, input: RouteInput)
        -> Result<Route, OagwError>;

    async fn get_route(&self, caller: &Caller, id: Uuid) -> Result<Route, OagwError>;

    async fn list_routes(
        &self,
        caller: &Caller,
        opts: &ListOptions,
    ) -> Result<(Vec<Route>, usize), OagwError>;

    async fn update_route(
        &self,
        caller: &Caller,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, OagwError>;

    async fn delete_route(&self, caller: &Caller, id: Uuid) -> Result<(), OagwError>;

    // -- custom plugins ----------------------------------------------------

    async fn create_plugin(
        &self,
        caller: &Caller,
        input: PluginInput,
    ) -> Result<PluginDef, OagwError>;

    async fn get_plugin(&self, caller: &Caller, id: Uuid) -> Result<PluginDef, OagwError>;

    async fn list_plugins(
        &self,
        caller: &Caller,
        opts: &ListOptions,
    ) -> Result<(Vec<PluginDef>, usize), OagwError>;

    async fn delete_plugin(&self, caller: &Caller, id: Uuid) -> Result<(), OagwError>;

    async fn get_plugin_source(
        &self,
        caller: &Caller,
        id: Uuid,
    ) -> Result<Option<String>, OagwError>;

    // -- routing resolution (used by the data plane) ------------------------

    /// Resolve the effective upstream for `alias` from this tenant chain.
    /// Returns `LinkUnavailable` for a disabled upstream, `NotFound` when no
    /// upstream binds the alias.
    async fn resolve_alias(
        &self,
        caller: &Caller,
        alias: &str,
    ) -> Result<UpstreamResolution, OagwError>;

    /// Resolve the best matching route for a request against `resolution`'s
    /// upstream. Matching is method allow-list + longest path prefix.
    async fn resolve_route(
        &self,
        caller: &Caller,
        resolution: &UpstreamResolution,
        method: &str,
        path: &str,
        query_keys: &[String],
    ) -> Result<Option<RouteResolution>, OagwError>;

    /// The rate limits forming the enforcement chain for `resolution`:
    /// selected upstream's limit, enforced/inherited ancestor limits, and
    /// `route_limit`. `None` entries are skipped so callers take the min.
    fn rate_limit_chain(
        &self,
        resolution: &UpstreamResolution,
        route_limit: Option<&RateLimitConfig>,
    ) -> Vec<RateLimitConfig>;

    /// Merged CORS configuration (union across the ancestor chain).
    fn effective_cors(&self, resolution: &UpstreamResolution) -> CorsConfig;

    /// Merged plugin bindings: ancestors root-first, then the selected
    /// upstream's own bindings, then the matched route's bindings (upstream
    /// plugins execute before route plugins, DESIGN.md plugin system).
    fn merged_plugins(
        &self,
        resolution: &UpstreamResolution,
        route_plugins: &PluginChainConfig,
    ) -> Vec<PluginBinding>;
}
