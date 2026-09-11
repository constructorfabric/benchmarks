//! The control plane: configuration CRUD and the resolution rules the data plane needs.

use std::sync::Arc;

use uuid::Uuid;

use super::alias;
use super::error::DomainError;
use super::gts_helpers;
use super::model::{Plugin, PluginRef, Route, Upstream};
use super::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Control plane over injected repositories.
pub struct ControlPlane {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    /// Legalises `scheme: "http"` upstream endpoints when set.
    allow_http: bool,
}

impl ControlPlane {
    /// Build a control plane over the given repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        allow_http: bool,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            allow_http,
        }
    }

    /// Legalises `scheme: "http"` upstream endpoints when set.
    #[must_use]
    pub fn allow_http(&self) -> bool {
        self.allow_http
    }

    // ---- upstreams -------------------------------------------------------

    /// Create an upstream, deriving or validating its alias.
    ///
    /// `requested_alias` is the caller-supplied value; when absent the alias is derived from the
    /// endpoints, and when present it must equal the derived alias (DESIGN §5.5).
    pub fn create_upstream(
        &self,
        mut upstream: Upstream,
        requested_alias: Option<String>,
    ) -> Result<Upstream, DomainError> {
        super::validation::validate_endpoints(&upstream.server.endpoints, self.allow_http)?;
        let effective = self.effective_alias(&upstream, requested_alias, None)?;
        upstream.alias = effective;
        if self
            .upstreams
            .alias_taken(upstream.tenant_id, &upstream.alias, None)
        {
            return Err(DomainError::AliasConflict(format!(
                "alias '{}' is already in use in this tenant",
                upstream.alias
            )));
        }
        super::validation::validate_upstream(&upstream, &self.plugins.list(None))?;
        self.upstreams.insert(upstream)
    }

    /// Fetch an upstream by tenant and id.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .get(tenant_id, id)
            .ok_or_else(|| missing_upstream(id))
    }

    /// Fetch an upstream by alias.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        self.upstreams.find_by_alias(tenant_id, alias)
    }

    /// List upstreams, optionally scoped to a tenant.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Option<Uuid>) -> Vec<Upstream> {
        self.upstreams.list(tenant_id)
    }

    /// Replace an upstream. `upstream_id` is immutable; alias immutability follows DESIGN §5.5.
    pub fn replace_upstream(
        &self,
        mut upstream: Upstream,
        requested_alias: Option<String>,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(upstream.tenant_id, upstream.id)?;
        super::validation::validate_endpoints(&upstream.server.endpoints, self.allow_http)?;
        let effective = self.effective_alias(&upstream, requested_alias, Some(&existing))?;
        upstream.alias = effective;
        if upstream.tenant_id != existing.tenant_id {
            return Err(missing_upstream(upstream.id));
        }
        if self
            .upstreams
            .alias_taken(upstream.tenant_id, &upstream.alias, Some(upstream.id))
        {
            return Err(DomainError::AliasConflict(format!(
                "alias '{}' is already in use in this tenant",
                upstream.alias
            )));
        }
        super::validation::validate_upstream(&upstream, &self.plugins.list(None))?;
        self.upstreams.update(upstream)
    }

    /// Delete an upstream and the routes bound to it.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        for route in self.routes.list_by_upstream(id) {
            if route.tenant_id == tenant_id {
                self.routes.delete(tenant_id, route.id)?;
            }
        }
        self.upstreams.delete(tenant_id, id).map(|_| ())
    }

    /// Enable or disable an upstream.
    pub fn set_upstream_enabled(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, DomainError> {
        let mut upstream = self.get_upstream(tenant_id, id)?;
        upstream.enabled = enabled;
        self.upstreams.update(upstream.clone())?;
        Ok(upstream)
    }

    /// Resolve the alias an upstream should carry.
    ///
    /// A caller-supplied alias must match the derived one when the derivation succeeds, and may
    /// never change on update. Otherwise the derivation (unchanged on update) is used.
    fn effective_alias(
        &self,
        upstream: &Upstream,
        requested: Option<String>,
        existing: Option<&Upstream>,
    ) -> Result<String, DomainError> {
        let derived = alias::derive_alias(&upstream.server.endpoints).ok();
        if let Some(provided) = requested {
            let provided = alias::normalize_alias(&provided);
            if let Some(derived_alias) = &derived
                && provided != *derived_alias
            {
                return Err(DomainError::Validation(format!(
                    "alias '{provided}' does not match the derived alias '{derived_alias}'"
                )));
            }
            if let Some(existing) = existing
                && !alias::aliases_equal(&provided, &existing.alias)
            {
                return Err(DomainError::Validation(format!(
                    "alias '{}' is immutable and cannot be changed to '{provided}'",
                    existing.alias
                )));
            }
            if derived.is_none() {
                return Ok(provided);
            }
            return Ok(provided);
        }
        match (&derived, existing) {
            // The alias is the routing key of the proxy path, so an endpoint change that would
            // rename the upstream is refused (DESIGN §Alias Update Behavior).
            (Some(derived_alias), Some(existing)) if !alias::aliases_equal(derived_alias, &existing.alias) => {
                Err(DomainError::Validation(format!(
                    "changing the endpoints would change the alias from '{}' to '{derived_alias}'; delete and re-create the upstream instead",
                    existing.alias
                )))
            }
            (Some(derived_alias), _) => Ok(derived_alias.clone()),
            (None, Some(existing)) => {
                // Hostname → IP is refused outright; IP → IP keeps the alias it already had.
                if alias::derive_alias(&existing.server.endpoints).is_ok() {
                    return Err(DomainError::Validation(format!(
                        "endpoints that do not derive an alias cannot replace the derivable alias '{}'; delete and re-create the upstream",
                        existing.alias
                    )));
                }
                Ok(existing.alias.clone())
            }
            (None, None) => Err(DomainError::Validation(
                "an explicit alias is required for these endpoints".to_string(),
            )),
        }
    }

    // ---- routes ---------------------------------------------------------

    /// Create a route.
    pub fn create_route(&self, route: Route) -> Result<Route, DomainError> {
        let upstream = self
            .upstreams
            .get(route.tenant_id, route.upstream_id)
            .ok_or_else(|| missing_upstream(route.upstream_id))?;
        super::validation::validate_route(&route, Some(&upstream), &self.plugins.list(None))?;
        if let Some(conflict) = self.routes.find_matching(route.upstream_id, &route.route_match) {
            return Err(DomainError::MatchConflict(format!(
                "route '{}' already claims this match rule",
                conflict.id
            )));
        }
        self.routes.insert(route)
    }

    /// Fetch a route.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes.get(tenant_id, id).ok_or_else(|| missing_route(id))
    }

    /// List routes, optionally scoped to a tenant.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Option<Uuid>) -> Vec<Route> {
        self.routes.list(tenant_id)
    }

    /// List routes bound to an upstream.
    #[must_use]
    pub fn list_routes_for_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        self.routes.list_by_upstream(upstream_id)
    }

    /// Replace a route. `upstream_id` is immutable.
    pub fn replace_route(&self, mut route: Route) -> Result<Route, DomainError> {
        let existing = self.get_route(route.tenant_id, route.id)?;
        if route.upstream_id != existing.upstream_id {
            return Err(DomainError::Validation(format!(
                "upstream_id is immutable and cannot be changed from '{}' to '{}'",
                existing.upstream_id, route.upstream_id
            )));
        }
        let upstream = self
            .upstreams
            .get(route.tenant_id, route.upstream_id)
            .ok_or_else(|| missing_upstream(route.upstream_id))?;
        super::validation::validate_route(&route, Some(&upstream), &self.plugins.list(None))?;
        if let Some(conflict) = self
            .routes
            .find_matching(route.upstream_id, &route.route_match)
            .filter(|c| c.id != route.id)
        {
            return Err(DomainError::MatchConflict(format!(
                "route '{}' already claims this match rule",
                conflict.id
            )));
        }
        route.upstream_id = existing.upstream_id;
        self.routes.update(route)
    }

    /// Delete a route.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.routes.delete(tenant_id, id).map(|_| ())
    }

    /// Enable or disable a route.
    pub fn set_route_enabled(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Route, DomainError> {
        let mut route = self.get_route(tenant_id, id)?;
        route.enabled = enabled;
        self.routes.update(route.clone())?;
        Ok(route)
    }

    // ---- plugins --------------------------------------------------------

    /// Create a custom plugin definition.
    pub fn create_plugin(&self, plugin: Plugin) -> Result<Plugin, DomainError> {
        super::validation::validate_plugin(&plugin)?;
        self.plugins.insert(plugin)
    }

    /// Fetch a custom plugin definition.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins.get(tenant_id, id).ok_or_else(|| missing_plugin(id))
    }

    /// List custom plugin definitions, optionally scoped to a tenant.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Option<Uuid>) -> Vec<Plugin> {
        self.plugins.list(tenant_id)
    }

    /// Replace a plugin definition.
    pub fn replace_plugin(&self, plugin: Plugin) -> Result<Plugin, DomainError> {
        super::validation::validate_plugin(&plugin)?;
        self.get_plugin(plugin.tenant_id, plugin.id)?;
        self.plugins.update(plugin)
    }

    /// Delete a plugin, refusing while an upstream or route still references it.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.get_plugin(tenant_id, id)?;
        let referenced = self.referencing_resources(tenant_id, id);
        if !referenced.is_empty() {
            return Err(DomainError::PluginInUse(format!(
                "plugin '{}' is still referenced by [{}]",
                id,
                referenced.join(", ")
            )));
        }
        self.plugins.delete(tenant_id, id).map(|_| ())
    }

    /// GTS ids of the upstreams and routes referencing `plugin_id`.
    #[must_use]
    pub fn referencing_resources(&self, tenant_id: Uuid, plugin_id: Uuid) -> Vec<String> {
        let needle = plugin_id.to_string();
        let matches_ref = |r: &PluginRef| ref_target(r) == Some(needle.as_str());
        let mut refs = Vec::new();
        for upstream in self.upstreams.list(Some(tenant_id)) {
            let Some(plugins) = &upstream.plugins else {
                continue;
            };
            if plugins.items.iter().any(&matches_ref) {
                refs.push(gts_helpers::upstream_gts(upstream.id));
            }
        }
        for route in self.routes.list(Some(tenant_id)) {
            let Some(plugins) = &route.plugins else {
                continue;
            };
            if plugins.items.iter().any(&matches_ref) {
                refs.push(gts_helpers::route_gts(route.id));
            }
        }
        refs
    }

    /// The plugins a tenant can bind: the built-in catalogue plus its own definitions.
    #[must_use]
    pub fn plugin_catalogue(&self, tenant_id: Option<Uuid>) -> Vec<Plugin> {
        self.plugins.list(tenant_id)
    }

    // ---- resolution -----------------------------------------------------

    /// Resolve `alias` in the tenant chain from the caller outward; the closest match wins.
    #[must_use]
    pub fn resolve_alias(&self, tenant_id: Uuid, chain: &[Uuid], alias: &str) -> Option<Upstream> {
        let mut ordered = vec![tenant_id];
        for t in chain {
            if *t != tenant_id {
                ordered.push(*t);
            }
        }
        for tenant in ordered {
            if let Some(found) = self.upstreams.find_by_alias(tenant, alias) {
                return Some(found);
            }
        }
        None
    }

    /// All enabled routes of `upstream_id`.
    #[must_use]
    pub fn enabled_routes(&self, upstream_id: Uuid) -> Vec<Route> {
        self.routes
            .list_by_upstream(upstream_id)
            .into_iter()
            .filter(|r| r.enabled)
            .collect()
    }

    /// The route whose match rule claims `method` and `path`, longest prefix first.
    #[must_use]
    pub fn match_route<'a>(
        &self,
        candidates: &'a [Route],
        method: &str,
        path: &str,
    ) -> Option<&'a Route> {
        let mut best: Option<(&Route, usize)> = None;
        for route in candidates {
            let Some(http) = route.route_match.as_http() else {
                continue;
            };
            if !http.methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
                continue;
            }
            let prefix = http.path.trim_end_matches('/');
            let matches = path == prefix
                || path.starts_with(&format!("{prefix}/"))
                || (prefix.is_empty() && path.starts_with('/'));
            if !matches {
                continue;
            }
            let depth = prefix.matches('/').count();
            if best.is_none_or(|(_, best_depth)| depth > best_depth) {
                best = Some((route, depth));
            }
        }
        best.map(|(route, _)| route)
    }
}

fn ref_target(r: &PluginRef) -> Option<&str> {
    match r {
        PluginRef::Bare(s) => Some(s.as_str()),
        PluginRef::Detailed { plugin_ref, .. } => Some(plugin_ref.as_str()),
    }
}

fn missing_upstream(id: Uuid) -> DomainError {
    DomainError::UpstreamNotFound(format!("upstream '{id}' does not exist"))
}

fn missing_route(id: Uuid) -> DomainError {
    DomainError::RouteNotFound(format!("route '{id}' does not exist"))
}

fn missing_plugin(id: Uuid) -> DomainError {
    DomainError::PluginNotFound(format!("plugin '{id}' does not exist"))
}

/// The protocol constant re-exported for callers that only hold the control plane.
pub const HTTP_PROTOCOL: &str = gts_helpers::PROTOCOL_HTTP;
