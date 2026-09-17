//! The composed `oagw` service and its control plane.
//!
//! [`ControlPlaneService`] owns configuration CRUD and the resolution the
//! data plane needs: alias walking over the tenant chain (closest match wins,
//! shadowing) and route selection. Every write is scoped to the calling
//! tenant — ancestor resources are invisible to the management API (404) and
//! reachable only through the proxy-time chain walk.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::{enforce_alias_create, enforce_alias_update};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::hierarchy::TenantHierarchy;
use crate::domain::matching;
use crate::domain::model::{Plugin, PluginSpec, Route, RouteSpec, Upstream, UpstreamSpec};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::validation::{validate_route, validate_upstream};
use crate::infra::plugin::registry::{
    AuthPluginRegistry, GuardPluginRegistry, PluginKind, TransformPluginRegistry,
};

/// Repositories, registries and hierarchy of the control plane.
#[derive(Debug, Clone)]
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    auth_plugins: AuthPluginRegistry,
    guard_plugins: GuardPluginRegistry,
    transform_plugins: TransformPluginRegistry,
    hierarchy: Arc<dyn TenantHierarchy>,
}

/// A proxy request resolved to an upstream and one of its routes.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProxyTarget {
    /// Upstream the alias resolved to (closest match in the tenant chain).
    pub upstream: Upstream,
    /// Route selected for the method/path (may live in an ancestor tenant).
    pub route: Route,
    /// Path forwarded upstream.
    pub upstream_path: String,
}

impl ControlPlaneService {
    /// Composes the control plane from repositories and a tenant hierarchy,
    /// with the built-in plugin registries.
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
    ) -> Self {
        Self::with_registries(
            upstreams,
            routes,
            plugins,
            hierarchy,
            AuthPluginRegistry::with_builtins(PluginKind::Auth),
            GuardPluginRegistry::with_builtins(PluginKind::Guard),
            TransformPluginRegistry::with_builtins(PluginKind::Transform),
        )
    }

    /// Composes the control plane with explicit plugin registries.
    #[allow(clippy::too_many_arguments)]
    pub fn with_registries(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        auth_plugins: AuthPluginRegistry,
        guard_plugins: GuardPluginRegistry,
        transform_plugins: TransformPluginRegistry,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            auth_plugins,
            guard_plugins,
            transform_plugins,
            hierarchy,
        }
    }

    /// The auth plugin registry.
    #[must_use]
    pub const fn auth_plugins(&self) -> &AuthPluginRegistry {
        &self.auth_plugins
    }

    /// The guard plugin registry.
    #[must_use]
    pub const fn guard_plugins(&self) -> &GuardPluginRegistry {
        &self.guard_plugins
    }

    /// The transform plugin registry.
    #[must_use]
    pub const fn transform_plugins(&self) -> &TransformPluginRegistry {
        &self.transform_plugins
    }

    /// Resolves a plugin reference against the registry of `kind`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] for unknown or catalog-only
    /// references.
    pub fn resolve_plugin_ref(
        &self,
        kind: PluginKind,
        plugin_ref: &str,
        tenant_id: Uuid,
    ) -> Result<(), DomainError> {
        let registry = match kind {
            PluginKind::Auth => &self.auth_plugins,
            PluginKind::Guard => &self.guard_plugins,
            PluginKind::Transform => &self.transform_plugins,
        };
        registry
            .resolve(plugin_ref, self.plugins.as_ref(), tenant_id)
            .map(|_| ())
    }

    // ---------------------------------------------------------------- upstreams

    /// Creates an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ValidationError`] for invalid specifications,
    /// [`ErrorKind::Conflict`] when the alias is taken by this tenant and
    /// [`ErrorKind::TenantNotFound`] when the resolver rejects the tenant.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        mut spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        let alias = enforce_alias_create(&spec.server.endpoints, spec.alias.as_deref())?;
        spec.alias = Some(alias);
        validate_upstream(&spec)?;
        self.validate_upstream_plugins(tenant_id, &spec)?;

        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            spec,
        };
        // A descendant alias that matches an ancestor's is a *bind*: allowed
        // unless the ancestor enforces its configuration, in which case the
        // override is refused. The `oagw:upstream:bind` permission itself is
        // enforced by the platform authorization layer.
        if self
            .shadowed_ancestor(ctx, tenant_id, upstream.alias())
            .await?
            .is_some_and(|ancestor| ancestor.enforces())
        {
            return Err(DomainError::new(
                ErrorKind::Conflict,
                format!(
                    "alias `{}` is enforced by an ancestor upstream and cannot be overridden",
                    upstream.alias()
                ),
            ));
        }
        self.upstreams.insert(&upstream)?;
        Ok(upstream)
    }

    /// Reads an upstream of this tenant.
    ///
    /// # Errors
    /// Returns [`ErrorKind::UpstreamNotFound`] when the tenant owns no such
    /// upstream.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams.find(tenant_id, id)?.ok_or_else(|| {
            DomainError::new(
                ErrorKind::UpstreamNotFound,
                format!("upstream {} does not exist", gts_upstream(id)),
            )
        })
    }

    /// Lists the upstreams of this tenant, ordered by alias.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list(tenant_id)
    }

    /// Replaces an upstream in place.
    ///
    /// # Errors
    /// Returns [`ErrorKind::UpstreamNotFound`], alias transition errors and
    /// validation errors.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        id: Uuid,
        mut spec: UpstreamSpec,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, id)?;
        let alias = enforce_alias_update(
            &spec.server.endpoints,
            existing.alias(),
            spec.alias.as_deref(),
        )?;
        spec.alias = Some(alias);
        validate_upstream(&spec)?;
        self.validate_upstream_plugins(tenant_id, &spec)?;

        let upstream = Upstream {
            id,
            tenant_id,
            spec,
        };
        if upstream.alias() != existing.alias()
            && self
                .shadowed_ancestor(ctx, tenant_id, upstream.alias())
                .await?
                .is_some()
        {
            return Err(DomainError::new(
                ErrorKind::Conflict,
                format!(
                    "alias `{}` is already provided by an ancestor",
                    upstream.alias()
                ),
            ));
        }
        self.upstreams.update(&upstream)?;
        Ok(upstream)
    }

    /// Deletes an upstream together with its routes.
    ///
    /// # Errors
    /// Returns [`ErrorKind::UpstreamNotFound`] when the upstream does not
    /// exist.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, id)?;
        for route in self.routes.list(tenant_id, Some(id))? {
            self.routes.delete(tenant_id, route.id)?;
        }
        self.upstreams.delete(tenant_id, id)?;
        Ok(existing)
    }

    // ------------------------------------------------------------------- routes

    /// Creates a route owned by `tenant_id`, referencing `upstream_id`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::UpstreamNotFound`] when the upstream does not
    /// belong to the calling tenant, [`ErrorKind::ValidationError`] for invalid
    /// rules and [`ErrorKind::Conflict`] when the match rule collides.
    pub fn create_route(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        self.get_upstream(tenant_id, upstream_id)?;
        validate_route(&spec)?;
        self.validate_route_plugins(tenant_id, &spec)?;
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            spec,
        };
        self.assert_match_unique(tenant_id, upstream_id, None, &route.spec.match_rule)?;
        self.routes.insert(&route)?;
        Ok(route)
    }

    /// Reads a route of this tenant.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the route does not exist.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes.find(tenant_id, id)?.ok_or_else(|| {
            DomainError::new(
                ErrorKind::ResourceNotFound,
                format!("route {} does not exist", gts_route(id)),
            )
        })
    }

    /// Lists the routes of this tenant, optionally filtered by upstream.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    pub fn list_routes(
        &self,
        tenant_id: Uuid,
        upstream_id: Option<Uuid>,
    ) -> Result<Vec<Route>, DomainError> {
        self.routes.list(tenant_id, upstream_id)
    }

    /// Replaces a route; `upstream_id` stays immutable.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`], validation and conflict errors.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: RouteSpec,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id)?;
        validate_route(&spec)?;
        self.validate_route_plugins(tenant_id, &spec)?;
        let route = Route {
            id,
            tenant_id,
            upstream_id: existing.upstream_id,
            spec,
        };
        self.assert_match_unique(
            tenant_id,
            existing.upstream_id,
            Some(id),
            &route.spec.match_rule,
        )?;
        self.routes.update(&route)?;
        Ok(route)
    }

    /// Deletes a route.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the route does not exist.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id)?;
        self.routes.delete(tenant_id, id)?;
        Ok(existing)
    }

    // ------------------------------------------------------------------ plugins

    /// Creates a custom plugin.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ValidationError`] for an unknown plugin type.
    pub fn create_plugin(&self, tenant_id: Uuid, spec: PluginSpec) -> Result<Plugin, DomainError> {
        validate_plugin_type(&spec.plugin_type)?;
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            spec,
        };
        self.plugins.insert(&plugin)?;
        Ok(plugin)
    }

    /// Reads a custom plugin.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the plugin does not exist.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins.find(tenant_id, id)?.ok_or_else(|| {
            DomainError::new(
                ErrorKind::ResourceNotFound,
                format!("plugin {} does not exist", gts_plugin(id)),
            )
        })
    }

    /// Lists the custom plugins of this tenant.
    ///
    /// # Errors
    /// Returns [`DomainError`] on storage failure.
    pub fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        self.plugins.list(tenant_id)
    }

    /// Deletes a plugin that is no longer referenced.
    ///
    /// # Errors
    /// Returns [`ErrorKind::ResourceNotFound`] when the plugin does not exist
    /// and [`ErrorKind::PluginInUse`] when an upstream or route still binds it.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        let existing = self.get_plugin(tenant_id, id)?;
        let reference = existing.id.to_string();
        let referenced = self
            .upstreams
            .list(tenant_id)?
            .into_iter()
            .any(|upstream| binds(&upstream.spec.plugins, &reference))
            || self
                .routes
                .list(tenant_id, None)?
                .into_iter()
                .any(|route| binds(&route.spec.plugins, &reference));
        if referenced {
            return Err(DomainError::new(
                ErrorKind::PluginInUse,
                format!(
                    "plugin {} is still referenced by an upstream or route",
                    existing.id
                ),
            ));
        }
        self.plugins.delete(tenant_id, id)?;
        Ok(existing)
    }

    // --------------------------------------------------------------- resolution

    /// Walks the tenant chain (descendant → root) for the closest upstream
    /// with `alias`.
    ///
    /// # Errors
    /// Returns [`ErrorKind::UpstreamNotFound`] when no tenant of the chain
    /// publishes the alias.
    pub async fn resolve_alias(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Upstream, DomainError> {
        for tenant in self.hierarchy.chain_from(ctx, tenant_id).await? {
            if self
                .upstreams
                .find_by_alias(tenant, alias)?
                .is_some_and(|upstream| upstream.spec.enabled)
            {
                return Ok(self
                    .upstreams
                    .find_by_alias(tenant, alias)?
                    .expect("just found"));
            }
        }
        Err(DomainError::new(
            ErrorKind::UpstreamNotFound,
            format!("no enabled upstream with alias `{alias}` is reachable from this tenant"),
        ))
    }

    /// Selects the route of `upstream_id` matching `method` and `suffix`,
    /// searching the tenant chain from the caller outwards.
    ///
    /// # Errors
    /// Returns [`ErrorKind::RouteNotFound`] when no route matches.
    pub fn resolve_route(
        &self,
        chain: &[Uuid],
        upstream_id: Uuid,
        method: &str,
        suffix: &str,
    ) -> Result<(Route, String), DomainError> {
        for tenant in chain {
            let routes = self.routes.list(*tenant, Some(upstream_id))?;
            if let Some(route) = matching::select(&routes, method, suffix) {
                let matched = matching::match_http(route, method, suffix)
                    .expect("the selected route matched");
                return Ok((route.clone(), matched.upstream_path));
            }
        }
        Err(DomainError::new(
            ErrorKind::RouteNotFound,
            format!(
                "no route of upstream {} matches {method} `{suffix}`",
                gts_upstream(upstream_id)
            ),
        ))
    }

    /// Resolves a proxy request to its upstream and route.
    ///
    /// # Errors
    /// Returns [`ErrorKind::UpstreamNotFound`] and [`ErrorKind::RouteNotFound`].
    pub async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
        method: &str,
        suffix: &str,
    ) -> Result<ResolvedProxyTarget, DomainError> {
        let upstream = self.resolve_alias(ctx, tenant_id, alias).await?;
        let chain = self.hierarchy.chain_from(ctx, tenant_id).await?;
        let (route, upstream_path) = self.resolve_route(&chain, upstream.id, method, suffix)?;
        Ok(ResolvedProxyTarget {
            upstream,
            route,
            upstream_path,
        })
    }

    /// The tenant chain of `tenant_id`, descendant → root.
    ///
    /// # Errors
    /// Returns [`ErrorKind::TenantNotFound`] when the resolver rejects the
    /// tenant.
    pub async fn tenant_chain(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        self.hierarchy.chain_from(ctx, tenant_id).await
    }

    // ------------------------------------------------------------------ helpers

    /// Returns the ancestor upstream shadowed by `alias`, when one exists.
    async fn shadowed_ancestor(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Option<Upstream>, DomainError> {
        for tenant in self.hierarchy.chain_from(ctx, tenant_id).await? {
            if tenant == tenant_id {
                continue;
            }
            if let Some(upstream) = self.upstreams.find_by_alias(tenant, alias)? {
                return Ok(Some(upstream));
            }
        }
        Ok(None)
    }

    fn assert_match_unique(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        exclude: Option<Uuid>,
        match_rule: &crate::domain::model::MatchConfig,
    ) -> Result<(), DomainError> {
        let collides = self
            .routes
            .list(tenant_id, Some(upstream_id))?
            .into_iter()
            .filter(|route| Some(route.id) != exclude)
            .any(|route| matching::collides(match_rule, &route.spec.match_rule));
        if collides {
            return Err(DomainError::new(
                ErrorKind::Conflict,
                "another route of this upstream already matches this path and method",
            ));
        }
        Ok(())
    }

    fn validate_upstream_plugins(
        &self,
        tenant_id: Uuid,
        spec: &UpstreamSpec,
    ) -> Result<(), DomainError> {
        if let Some(auth) = &spec.auth {
            self.resolve_plugin_ref(PluginKind::Auth, &auth.auth_type, tenant_id)?;
        }
        self.validate_bound_items(tenant_id, spec.plugins.as_ref())
    }

    fn validate_route_plugins(&self, tenant_id: Uuid, spec: &RouteSpec) -> Result<(), DomainError> {
        self.validate_bound_items(tenant_id, spec.plugins.as_ref())
    }

    fn validate_bound_items(
        &self,
        tenant_id: Uuid,
        plugins: Option<&crate::domain::model::PluginsConfig>,
    ) -> Result<(), DomainError> {
        let Some(plugins) = plugins else {
            return Ok(());
        };
        for item in &plugins.items {
            let reference = item.reference();
            self.resolve_plugin_ref(PluginKind::Guard, reference, tenant_id)
                .or_else(|_| {
                    self.resolve_plugin_ref(PluginKind::Transform, reference, tenant_id)
                })?;
        }
        Ok(())
    }
}

/// Whether a plugin binding list references the bare plugin id.
fn binds(plugins: &Option<crate::domain::model::PluginsConfig>, reference: &str) -> bool {
    plugins.as_ref().is_some_and(|plugins| {
        plugins
            .items
            .iter()
            .any(|item| crate::gts_helpers::plugin_instance(item.reference()) == reference)
    })
}

fn validate_plugin_type(plugin_type: &str) -> Result<(), DomainError> {
    let valid = matches!(
        plugin_type,
        "auth_plugin" | "guard_plugin" | "transform_plugin"
    );
    if valid {
        Ok(())
    } else {
        Err(DomainError::new(
            ErrorKind::ValidationError,
            format!("invalid plugin type `{plugin_type}`"),
        ))
    }
}

fn gts_upstream(id: Uuid) -> String {
    crate::gts_helpers::resource_id_to_gts(crate::gts_helpers::OagwResourceKind::Upstream, id)
}

fn gts_route(id: Uuid) -> String {
    crate::gts_helpers::resource_id_to_gts(crate::gts_helpers::OagwResourceKind::Route, id)
}

fn gts_plugin(id: Uuid) -> String {
    crate::gts_helpers::resource_id_to_gts(crate::gts_helpers::OagwResourceKind::Plugin, id)
}

/// The composed gear service handed to the REST layer.
#[derive(Debug, Clone)]
pub struct OagwService {
    config: OagwConfig,
    control_plane: ControlPlaneService,
}

impl OagwService {
    /// Composes the service from the gear configuration and a tenant
    /// hierarchy source.
    #[must_use]
    pub fn new(config: OagwConfig, hierarchy: Arc<dyn TenantHierarchy>) -> Self {
        Self {
            config,
            control_plane: ControlPlaneService::new(
                Arc::new(crate::infra::storage::InMemoryUpstreamRepo::new()),
                Arc::new(crate::infra::storage::InMemoryRouteRepo::new()),
                Arc::new(crate::infra::storage::InMemoryPluginRepo::new()),
                hierarchy,
            ),
        }
    }

    /// Composes the service from explicit parts (used by tests).
    #[must_use]
    pub const fn from_parts(config: OagwConfig, control_plane: ControlPlaneService) -> Self {
        Self {
            config,
            control_plane,
        }
    }

    /// The gear configuration.
    #[must_use]
    pub const fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// The control plane.
    #[must_use]
    pub const fn control_plane(&self) -> &ControlPlaneService {
        &self.control_plane
    }
}

#[cfg(test)]
mod tests;
