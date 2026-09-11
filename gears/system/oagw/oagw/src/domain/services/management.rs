//! Control Plane service: CRUD for upstreams, routes and plugins.
//!
//! Every operation is tenant-scoped from the extracted [`crate::api::extract`]
//! security context; the service never reads a tenant from a body. Alias
//! derivation, wire-contract validation, uniqueness and plugin reference
//! counting all live here so the handlers stay thin.

use crate::domain::error::{DomainError, OagwError};
use crate::domain::identifiers;
use crate::domain::model::{Plugin, PluginRef, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::{alias, validation};
use crate::gts_helpers;
use std::sync::Arc;
use uuid::Uuid;

/// Reads one resource, trying each spelling of its identifier.
///
/// The first candidate that resolves wins; otherwise the last failure is
/// returned, so a not-found carries the resource the caller actually named.
macro_rules! lookup {
    ($tenant:expr, $id:expr, $prefix:expr, $repository:expr) => {{
        let repository = Arc::clone(&$repository);
        let mut last: Option<DomainError> = None;
        let mut found = None;
        for candidate in candidate_ids($id, $prefix) {
            match repository.get($tenant, &candidate).await {
                Ok(hit) => {
                    found = Some(hit);
                    break;
                }
                Err(err) => last = Some(err),
            }
        }
        match found {
            Some(hit) => Ok(hit),
            None => Err(OagwError::from(last.unwrap_or_else(|| {
                DomainError::NotFound {
                    kind: "resource".to_owned(),
                    target: $id.to_owned(),
                }
            }))),
        }
    }};
}

/// Items returned by a list call when the caller passes no `$top`.
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Largest page size a caller may request.
pub const MAX_PAGE_SIZE: usize = 100;

/// The Control Plane: management CRUD over the repository ports.
#[derive(Clone)]
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
}

impl ControlPlaneService {
    /// Builds a service over the given repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
        }
    }

    /// The upstream repository, for the data plane's alias walk.
    #[must_use]
    pub fn upstreams(&self) -> &Arc<dyn UpstreamRepository> {
        &self.upstreams
    }

    /// The route repository, for the data plane's route match.
    #[must_use]
    pub fn routes(&self) -> &Arc<dyn RouteRepository> {
        &self.routes
    }

    /// The plugin repository.
    #[must_use]
    pub fn plugins(&self) -> &Arc<dyn PluginRepository> {
        &self.plugins
    }

    /// Creates an upstream, deriving or validating its alias.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] for an invalid body or a
    /// contradictory alias, [`OagwError::Conflict`] for a taken alias.
    pub async fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, OagwError> {
        let supplied = non_empty(&upstream.alias);
        upstream.alias = alias::resolve_for_create(&upstream.server, supplied)?;
        upstream.id = identifiers::typed_id(gts_helpers::UPSTREAM_TYPE);
        upstream.tenant_id = tenant_id;
        validation::validate_upstream(&upstream)?;
        stamp_created(&mut upstream);
        self.upstreams
            .create(upstream)
            .await
            .map_err(OagwError::from)
    }

    /// Lists the tenant's upstreams in alias order.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory repository.
    pub async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, OagwError> {
        self.upstreams
            .list(tenant_id)
            .await
            .map_err(OagwError::from)
    }

    /// Reads one upstream, accepting typed or bare identifiers.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent or foreign.
    pub async fn get_upstream(&self, tenant_id: Uuid, id: &str) -> Result<Upstream, OagwError> {
        lookup!(tenant_id, id, gts_helpers::UPSTREAM_TYPE, self.upstreams)
    }

    /// Replaces an upstream, enforcing alias immutability.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] for an invalid body or an alias
    /// change, [`OagwError::RouteNotFound`] when absent.
    pub async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: &str,
        mut upstream: Upstream,
    ) -> Result<Upstream, OagwError> {
        let current = self.get_upstream(tenant_id, id).await?;
        let supplied = non_empty(&upstream.alias);
        alias::enforce_alias_update(&current.alias, &upstream.server, supplied)?;
        upstream.id = current.id.clone();
        upstream.tenant_id = tenant_id;
        upstream.alias = current.alias.clone();
        upstream.created_at = current.created_at.clone();
        validation::validate_upstream(&upstream)?;
        upstream.updated_at = Some(identifiers::now_rfc3339());
        self.upstreams
            .replace(upstream)
            .await
            .map_err(OagwError::from)
    }

    /// Deletes an upstream and every route under it.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let upstream = self.get_upstream(tenant_id, id).await?;
        self.routes
            .delete_by_upstream(tenant_id, &upstream.id)
            .await
            .map_err(OagwError::from)?;
        self.upstreams
            .delete(tenant_id, &upstream.id)
            .await
            .map_err(OagwError::from)
    }

    /// Creates a route under an in-tenant upstream.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] for an invalid body or an
    /// out-of-tenant upstream, [`OagwError::Conflict`] for a duplicate rule.
    pub async fn create_route(
        &self,
        tenant_id: Uuid,
        upstream_reference: &str,
        mut route: Route,
    ) -> Result<Route, OagwError> {
        let upstream_id = reference_id(upstream_reference, gts_helpers::UPSTREAM_TYPE);
        self.upstreams
            .get(tenant_id, &upstream_id)
            .await
            .map_err(|_| {
                OagwError::ValidationError(format!(
                    "upstream_id '{upstream_id}' must name an upstream of the calling tenant"
                ))
            })?;
        route.upstream_id = upstream_id;
        route.id = identifiers::typed_id(gts_helpers::ROUTE_TYPE);
        route.tenant_id = tenant_id;
        validation::validate_route(&route)?;
        stamp_route_created(&mut route);
        self.routes.create(route).await.map_err(OagwError::from)
    }

    /// Lists the tenant's routes.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory repository.
    pub async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, OagwError> {
        self.routes.list(tenant_id).await.map_err(OagwError::from)
    }

    /// Reads one route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent or foreign.
    pub async fn get_route(&self, tenant_id: Uuid, id: &str) -> Result<Route, OagwError> {
        lookup!(tenant_id, id, gts_helpers::ROUTE_TYPE, self.routes)
    }

    /// Replaces a route; `upstream_id` stays immutable.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] for an invalid body,
    /// [`OagwError::RouteNotFound`] when absent,
    /// [`OagwError::Conflict`] on a duplicate rule.
    pub async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: &str,
        mut route: Route,
    ) -> Result<Route, OagwError> {
        let current = self.get_route(tenant_id, id).await?;
        route.id = current.id.clone();
        route.tenant_id = tenant_id;
        route.upstream_id = current.upstream_id.clone();
        validation::validate_route(&route)?;
        route.created_at = current.created_at.clone();
        route.updated_at = Some(identifiers::now_rfc3339());
        self.routes.replace(route).await.map_err(OagwError::from)
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent.
    pub async fn delete_route(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let route = self.get_route(tenant_id, id).await?;
        self.routes
            .delete(tenant_id, &route.id)
            .await
            .map_err(OagwError::from)
    }

    /// Creates a custom plugin; built-ins are never persisted.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::ValidationError`] for an invalid body,
    /// [`OagwError::Conflict`] for a taken name.
    pub async fn create_plugin(
        &self,
        tenant_id: Uuid,
        mut plugin: Plugin,
    ) -> Result<Plugin, OagwError> {
        plugin.id = identifiers::typed_id(plugin.plugin_type.type_prefix());
        plugin.tenant_id = tenant_id;
        if plugin.source_code.is_empty() {
            return Err(OagwError::ValidationError(
                "source_code must not be empty".to_owned(),
            ));
        }
        self.plugins.create(plugin).await.map_err(OagwError::from)
    }

    /// Lists the tenant's custom plugins; built-ins are not listed.
    ///
    /// # Errors
    ///
    /// Never fails for the in-memory repository.
    pub async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, OagwError> {
        self.plugins.list(tenant_id).await.map_err(OagwError::from)
    }

    /// Reads one plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent (built-ins included).
    pub async fn get_plugin(&self, tenant_id: Uuid, id: &str) -> Result<Plugin, OagwError> {
        lookup!(tenant_id, id, gts_helpers::AUTH_PLUGIN_TYPE, self.plugins)
    }

    /// Reads a plugin's Starlark source.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent (built-ins included).
    pub async fn get_plugin_source(&self, tenant_id: Uuid, id: &str) -> Result<String, OagwError> {
        self.get_plugin(tenant_id, id)
            .await
            .map(|plugin| plugin.source_code)
    }

    /// Deletes an unreferenced plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when absent,
    /// [`OagwError::PluginInUse`] when an upstream or route still references
    /// it.
    pub async fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let plugin = self.get_plugin(tenant_id, id).await?;
        let mut upstream_refs = Vec::new();
        for upstream in self.upstreams.list(tenant_id).await? {
            if references(&upstream.plugins.items, &plugin.id) {
                upstream_refs.push(upstream.id.clone());
            }
        }
        let mut route_refs = Vec::new();
        for route in self.routes.list(tenant_id).await? {
            if references(&route.plugins.items, &plugin.id) {
                route_refs.push(route.id.clone());
            }
        }
        if !upstream_refs.is_empty() || !route_refs.is_empty() {
            return Err(OagwError::PluginInUse(format!(
                "plugin is referenced by {} upstream(s) and {} route(s)",
                upstream_refs.len(),
                route_refs.len()
            )));
        }
        self.plugins
            .delete(tenant_id, &plugin.id)
            .await
            .map_err(OagwError::from)
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Stamps the creation timestamps of a new resource.
fn stamp_created(upstream: &mut Upstream) {
    let now = identifiers::now_rfc3339();
    upstream.created_at = Some(now.clone());
    upstream.updated_at = Some(now);
}

/// Stamps the creation timestamps of a new route.
fn stamp_route_created(route: &mut Route) {
    let now = identifiers::now_rfc3339();
    route.created_at = Some(now.clone());
    route.updated_at = Some(now);
}

/// Normalises a bare or typed reference into the internal identifier form.
fn reference_id(value: &str, prefix: &str) -> String {
    if value.contains('~') {
        value.to_owned()
    } else {
        format!("{prefix}{value}")
    }
}

/// Tries each accepted spelling of an identifier until one resolves.
/// The typed and bare spellings of an identifier.
fn candidate_ids(id: &str, type_prefix: &str) -> Vec<String> {
    if id.contains('~') || type_prefix.is_empty() {
        vec![id.to_owned()]
    } else {
        vec![format!("{type_prefix}{id}"), id.to_owned()]
    }
}

/// Whether a plugin chain references the given plugin instance.
fn references(items: &[PluginRef], plugin_id: &str) -> bool {
    let bare = plugin_id.split('~').next_back().unwrap_or(plugin_id);
    items.iter().any(|item| {
        let candidate = item.id();
        candidate == plugin_id || candidate.ends_with(&format!("~{bare}"))
    })
}
