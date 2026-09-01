//! Control Plane service contract.
//!
//! Owns all configuration data (upstreams, routes, plugins), the tenant
//! hierarchy walk used at proxy time, and the alias resolution rules. The
//! transport layer calls this trait only; persistence is behind the
//! [`crate::domain::repo`] repository traits.

use async_trait::async_trait;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::merge::AncestorConfig;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};
use crate::domain::plugin::PluginChain;

/// Pagination / filtering parameters for list endpoints (OData `$top`,
/// `$skip`, `$filter`, `$orderby`, `$select`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// Maximum number of items (default 50, max 100).
    pub top: Option<u64>,
    /// Number of items to skip.
    pub skip: Option<u64>,
    /// OData filter expression.
    pub filter: Option<String>,
    /// Sort expression, e.g. `created_at desc`.
    pub orderby: Option<String>,
    /// Projected field names.
    pub select: Option<Vec<String>>,
}

impl ListQuery {
    /// Clamps `$top` to the documented default (50) and maximum (100).
    #[must_use]
    pub fn effective_top(&self) -> u64 {
        self.top.unwrap_or(50).clamp(1, 100)
    }
}

/// A resolved proxy target: the effective upstream, the matched route and the
/// effective (merged) configuration.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The upstream that was selected by alias shadowing.
    pub upstream: Upstream,
    /// Whether the selected upstream was owned by the calling tenant.
    pub inherited: bool,
    /// Ancestor chain (root → immediate parent of the selected upstream).
    pub chain: Vec<AncestorSnapshot>,
    /// The matched route.
    pub route: Route,
    /// Whether the matched route came from an ancestor tenant.
    pub route_inherited: bool,
    /// The effective plugin chain (upstream plugins before route plugins).
    pub plugin_chain: PluginChain,
    /// Effective rate limit, after hierarchical merge.
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
    /// Effective CORS configuration, after hierarchical merge.
    pub cors: Option<crate::domain::model::CorsConfig>,
    /// Effective header rules.
    pub headers: Option<crate::domain::model::HeadersConfig>,
    /// Whether the selected upstream alias was derived from a shared
    /// registrable suffix (drives the `X-OAGW-Target-Host` requirement).
    pub alias_is_common_suffix: bool,
}

/// An upstream snapshot from an ancestor tenant.
#[derive(Debug, Clone)]
pub struct AncestorSnapshot {
    /// Tenant the ancestor belongs to.
    pub tenant_id: uuid::Uuid,
    /// The ancestor upstream.
    pub upstream: crate::domain::model::Upstream,
}

impl AncestorSnapshot {
    /// Borrowed view used by the merge helpers.
    #[must_use]
    pub fn as_ancestor_config(&self) -> AncestorConfig<'_> {
        AncestorConfig {
            tenant_id: self.tenant_id,
            upstream: &self.upstream,
        }
    }
}

impl ResolvedTarget {
    /// The ancestor snapshots as merge inputs, root first.
    #[must_use]
    pub fn ancestors(&self) -> Vec<AncestorConfig<'_>> {
        self.chain
            .iter()
            .map(AncestorSnapshot::as_ancestor_config)
            .collect()
    }
}

/// Tenant-defined Starlark plugin source plus its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSource {
    /// GTS instance id of the plugin.
    pub plugin_id: String,
    /// Plugin type.
    pub plugin_type: PluginType,
    /// Starlark source code.
    pub source_code: String,
}

/// Control Plane service contract.
#[async_trait]
pub trait ControlPlaneService: Send + Sync {
    /// Creates an upstream for the calling tenant.
    ///
    /// # Errors
    ///
    /// Validation, alias derivation and uniqueness failures.
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        upstream: Upstream,
    ) -> Result<Upstream, DomainError>;

    /// Lists upstreams owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Malformed filter expressions.
    async fn list_upstreams(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, DomainError>;

    /// Fetches an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// 404 when the upstream is not owned by the caller (ancestors are
    /// invisible through the management API).
    async fn get_upstream(&self, ctx: &SecurityContext, id: &str) -> Result<Upstream, DomainError>;

    /// Replaces an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Validation and immutability failures.
    async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: &str,
        upstream: Upstream,
    ) -> Result<Upstream, DomainError>;

    /// Deletes an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// 404 when the upstream is not owned by the caller.
    async fn delete_upstream(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError>;

    /// Creates a route for the calling tenant.
    ///
    /// # Errors
    ///
    /// Validation, upstream ownership and match-uniqueness failures.
    async fn create_route(&self, ctx: &SecurityContext, route: Route)
    -> Result<Route, DomainError>;

    /// Lists routes owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Malformed filter expressions.
    async fn list_routes(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Route>, DomainError>;

    /// Fetches a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// 404 when the route is not owned by the caller.
    async fn get_route(&self, ctx: &SecurityContext, id: &str) -> Result<Route, DomainError>;

    /// Replaces a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// Validation and match-uniqueness failures.
    async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: &str,
        route: Route,
    ) -> Result<Route, DomainError>;

    /// Deletes a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// 404 when the route is not owned by the caller.
    async fn delete_route(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError>;

    /// Creates a tenant-defined plugin.
    ///
    /// # Errors
    ///
    /// Validation and name-uniqueness failures.
    async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        plugin: Plugin,
    ) -> Result<Plugin, DomainError>;

    /// Lists plugins of the calling tenant.
    ///
    /// # Errors
    ///
    /// Malformed filter expressions.
    async fn list_plugins(
        &self,
        ctx: &SecurityContext,
        query: &ListQuery,
        plugin_type: Option<PluginType>,
    ) -> Result<Vec<Plugin>, DomainError>;

    /// Fetches a plugin owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// 404 when the plugin is not owned by the caller.
    async fn get_plugin(&self, ctx: &SecurityContext, id: &str) -> Result<Plugin, DomainError>;

    /// Deletes a plugin, failing when it is still referenced.
    ///
    /// # Errors
    ///
    /// 409 [`DomainError::PluginInUse`] with the referencing resources.
    async fn delete_plugin(&self, ctx: &SecurityContext, id: &str) -> Result<(), DomainError>;

    /// Returns the Starlark source of a plugin owned by the caller.
    ///
    /// # Errors
    ///
    /// 404 when the plugin is not owned by the caller.
    async fn get_plugin_source(
        &self,
        ctx: &SecurityContext,
        id: &str,
    ) -> Result<PluginSource, DomainError>;

    /// Resolves the effective proxy target for an alias.
    ///
    /// Walks the tenant chain from the calling tenant to the root, selects the
    /// closest enabled upstream by alias (shadowing), matches a route across
    /// the chain (descendant routes take priority) and merges the effective
    /// configuration.
    ///
    /// # Errors
    ///
    /// 404 [`DomainError::RouteNotFound`] when the alias or the route is
    /// unknown or disabled.
    async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        method: &str,
        request_path: &str,
        path_suffix: &str,
    ) -> Result<ResolvedTarget, DomainError>;
}
