//! Control-plane contract: upstream/route/plugin configuration management
//! (DESIGN §3.3 "Management API").

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::OagwResult;
use crate::domain::model::{Plugin, PluginInput, Route, RouteInput, Upstream, UpstreamInput};
use crate::domain::services::proxy::{ResolvedTarget, TargetHostChoice};

/// OData-style list parameters shared by all list endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// `$filter` expression (supported predicates only).
    pub filter: Option<String>,
    /// `$select` field list.
    pub select: Option<Vec<String>>,
    /// `$orderby` expression, e.g. `created_at desc`.
    pub orderby: Option<String>,
    /// `$top` (default 50, max 100).
    pub top: Option<usize>,
    /// `$skip`.
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Effective page size, clamped to the documented maximum of 100.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.top.unwrap_or(50).min(100)
    }

    /// Effective offset.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.skip.unwrap_or(0)
    }
}

/// Result of a plugin-source lookup.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PluginSource {
    /// Plugin identifier.
    pub plugin_id: String,
    /// Plugin kind.
    pub plugin_type: String,
    /// Language the source is written in.
    pub language: &'static str,
    /// Where the implementation lives (builtin) or the Starlark body (custom).
    pub source: String,
    /// Plugin configuration as stored.
    pub config: serde_json::Value,
}

/// Control-plane operations (DESIGN §3.3).
#[async_trait]
pub trait ControlPlaneService: Send + Sync {
    // -- upstreams ---------------------------------------------------------
    /// Creates an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`] on invalid configuration,
    /// [`OagwError::AliasConflict`] when the alias is taken.
    async fn create_upstream(&self, ctx: &SecurityContext, input: UpstreamInput)
        -> OagwResult<Upstream>;

    /// Lists the calling tenant's upstreams.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`] on malformed list parameters.
    async fn list_upstreams(&self, ctx: &SecurityContext, query: &ListQuery)
        -> OagwResult<Vec<Upstream>>;

    /// Reads one upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`] when the upstream does not belong to the
    /// calling tenant.
    async fn get_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Upstream>;

    /// Replaces an upstream (full replacement; alias is recomputed).
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`], [`OagwError::Validation`],
    /// [`OagwError::AliasConflict`].
    async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamInput,
    ) -> OagwResult<Upstream>;

    /// Deletes an upstream.
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`], [`OagwError::RouteConflict`] when a route
    /// still references it.
    async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()>;

    // -- routes ------------------------------------------------------------
    /// Creates a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`], [`OagwError::RouteConflict`].
    async fn create_route(&self, ctx: &SecurityContext, input: RouteInput) -> OagwResult<Route>;

    /// Lists the calling tenant's routes.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`] on malformed list parameters.
    async fn list_routes(&self, ctx: &SecurityContext, query: &ListQuery)
        -> OagwResult<Vec<Route>>;

    /// Reads one route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`].
    async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Route>;

    /// Replaces a route (`upstream_id` is immutable).
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`], [`OagwError::RouteConflict`],
    /// [`OagwError::RouteNotFound`].
    async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteInput,
    ) -> OagwResult<Route>;

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`].
    async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()>;

    // -- plugins -----------------------------------------------------------
    /// Creates a custom plugin owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`].
    async fn create_plugin(&self, ctx: &SecurityContext, input: PluginInput)
        -> OagwResult<Plugin>;

    /// Lists built-in and custom plugins visible to the calling tenant.
    ///
    /// # Errors
    ///
    /// [`OagwError::Validation`] on malformed list parameters.
    async fn list_plugins(&self, ctx: &SecurityContext, query: &ListQuery)
        -> OagwResult<Vec<Plugin>>;

    /// Reads a plugin by id (builtin or custom).
    ///
    /// # Errors
    ///
    /// [`OagwError::PluginNotFound`].
    async fn get_plugin(&self, ctx: &SecurityContext, id: &str) -> OagwResult<Plugin>;

    /// Deletes a custom plugin.
    ///
    /// # Errors
    ///
    /// [`OagwError::PluginNotFound`], [`OagwError::PluginInUse`].
    async fn delete_plugin(&self, ctx: &SecurityContext, id: &str) -> OagwResult<()>;

    /// Returns the implementation source of a plugin.
    ///
    /// # Errors
    ///
    /// [`OagwError::PluginNotFound`].
    async fn get_plugin_source(&self, ctx: &SecurityContext, id: &str) -> OagwResult<PluginSource>;

    // -- data-plane support ------------------------------------------------
    /// Resolves an alias for the calling tenant (descendant → root walk).
    ///
    /// # Errors
    ///
    /// [`OagwError::RouteNotFound`] when no enabled upstream owns the alias in
    /// the tenant chain.
    async fn resolve_target(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        choice: &TargetHostChoice,
    ) -> OagwResult<ResolvedTarget>;
}
