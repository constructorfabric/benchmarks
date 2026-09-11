//! Control-plane and data-plane service contracts.

use async_trait::async_trait;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{ControlPlaneError, ControlPlaneResult};

/// A resolved upstream together with the tenant that owns it.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// The upstream.
    pub upstream: Upstream,
    /// How far up the hierarchy the match was found: `0` is the calling
    /// tenant, `1` its parent, and so on.
    pub depth: usize,
}

/// Control-plane operations: management `CRUD` plus alias resolution.
#[async_trait]
pub trait ControlPlaneService: Send + Sync {
    /// Creates an upstream.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError`] for validation, uniqueness and plugin
    /// resolution failures.
    async fn create_upstream(
        &self,
        tenant_id: uuid::Uuid,
        upstream: Upstream,
    ) -> ControlPlaneResult<Upstream>;

    /// Replaces an upstream.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError`] for validation and uniqueness failures.
    async fn replace_upstream(
        &self,
        tenant_id: uuid::Uuid,
        id: &str,
        upstream: Upstream,
    ) -> ControlPlaneResult<Upstream>;

    /// Reads an upstream.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when unknown.
    async fn get_upstream(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<Upstream>;

    /// Deletes an upstream together with its routes.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    async fn delete_upstream(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<()>;

    /// Lists upstreams owned by the tenant.
    async fn list_upstreams(&self, tenant_id: uuid::Uuid) -> Vec<Upstream>;

    /// Creates a route.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError`] for validation, uniqueness and reference
    /// failures.
    async fn create_route(
        &self,
        tenant_id: uuid::Uuid,
        route: Route,
    ) -> ControlPlaneResult<Route>;

    /// Replaces a route.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError`] for validation and uniqueness failures.
    async fn replace_route(
        &self,
        tenant_id: uuid::Uuid,
        id: &str,
        route: Route,
    ) -> ControlPlaneResult<Route>;

    /// Reads a route.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    async fn get_route(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<Route>;

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    async fn delete_route(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<()>;

    /// Lists routes owned by the tenant.
    async fn list_routes(&self, tenant_id: uuid::Uuid) -> Vec<Route>;

    /// Creates a custom plugin.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::Conflict`] when the name is taken.
    async fn create_plugin(
        &self,
        tenant_id: uuid::Uuid,
        name: &str,
        plugin_type: crate::domain::model::PluginType,
        source_code: &str,
    ) -> ControlPlaneResult<Plugin>;

    /// Reads a custom plugin.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::NotFound`] when the id is unknown.
    async fn get_plugin(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<Plugin>;

    /// Deletes a custom plugin.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::InUse`] when an upstream or route still
    /// references it.
    async fn delete_plugin(&self, tenant_id: uuid::Uuid, id: &str) -> ControlPlaneResult<()>;

    /// Lists custom plugins owned by the tenant.
    async fn list_plugins(&self, tenant_id: uuid::Uuid) -> Vec<Plugin>;

    /// Resolves an alias by walking the tenant hierarchy from `tenant_id`
    /// towards the root; the closest match wins.
    async fn resolve_alias(&self, tenant_id: uuid::Uuid, alias: &str) -> Option<ResolvedUpstream>;

    /// Validates that every `plugin_ref` in a chain resolves.
    ///
    /// # Errors
    ///
    /// Returns [`ControlPlaneError::UnknownPlugin`] when a reference cannot be
    /// resolved, and [`ControlPlaneError::Validation`] when it is not a `GTS`
    /// identifier at all.
    async fn validate_plugin_refs(
        &self,
        tenant_id: uuid::Uuid,
        refs: &[String],
    ) -> ControlPlaneResult<()>;

    /// Matches a request path and method against the resolved upstream's
    /// enabled routes.
    async fn match_route(
        &self,
        resolved: &ResolvedUpstream,
        method: &str,
        path_suffix: &str,
    ) -> Option<MatchedRoute>;

    /// Computes the effective rate limit for a resolved upstream and route.
    async fn effective_rate_limit(
        &self,
        tenant_id: uuid::Uuid,
        alias: &str,
        route: Option<&Route>,
    ) -> Option<crate::domain::model::RateLimit>;
}

/// Result of matching a request against the route table.
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The matched route.
    pub route: Route,
    /// Owning tenant of the matched route.
    pub tenant_id: uuid::Uuid,
    /// Depth in the hierarchy the route was found at.
    pub depth: usize,
}

/// Turns a [`ControlPlaneError`] into the documented `HTTP` error.
#[must_use]
pub fn control_plane_error(err: ControlPlaneError) -> crate::api::error::OagwError {
    use crate::api::error::OagwError;
    use crate::domain::ids;

    match err {
        ControlPlaneError::Validation(detail) => OagwError::validation(detail),
        // Management reads that miss are reported as validation failures so
        // the problem `type` is the documented `validation.error.v1`.
        ControlPlaneError::NotFound => OagwError::not_found(
            ids::ERR_VALIDATION,
            "Not Found",
            "the requested resource does not exist in this tenant",
        ),
        ControlPlaneError::Conflict(detail) => {
            OagwError::conflict(ids::ERR_ALIAS_CONFLICT, "Conflict", detail)
        }
        ControlPlaneError::RouteConflict(detail) => {
            OagwError::conflict(ids::ERR_ROUTE_CONFLICT, "Conflict", detail)
        }
        ControlPlaneError::InUse {
            referenced_by,
            detail,
        } => OagwError::conflict(ids::ERR_PLUGIN_IN_USE, "Plugin In Use", detail)
            .with_extension("referenced_by", referenced_by),
        ControlPlaneError::UnknownPlugin(reference) => {
            OagwError::validation(format!("unknown auth plugin {reference}"))
        }
        ControlPlaneError::UnknownUpstream => OagwError::validation("unknown upstream"),
    }
}
