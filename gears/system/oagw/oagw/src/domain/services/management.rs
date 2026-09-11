//! `ControlPlaneService` — CRUD for upstreams/routes/plugins plus the single
//! alias-resolution + route-matching walk used by the data plane
//! (DESIGN §3.2 "Internal Services", ADR 0006).

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::model::{
    CorsConfig, EndpointScheme, Plugin, PluginBinding, RateLimitConfig, Route, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, TenantHierarchy, UpstreamRepository};

/// The effective configuration a proxy request resolves to (ADR 0006).
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The selected upstream.
    pub upstream: Upstream,
    /// The matched route.
    pub route: Route,
    /// The part of the proxy path the route prefix did not cover.
    pub path_remainder: String,
    /// Guard/transform bindings: upstream plugins first, then route plugins.
    pub plugins: Vec<PluginBinding>,
    /// Effective rate limit (the strictest of upstream, route and enforced
    /// ancestors), when any applies.
    pub rate_limit: Option<RateLimitConfig>,
    /// Effective CORS policy.
    pub cors: Option<CorsConfig>,
    /// The tenant whose upstream was selected.
    pub owning_tenant: String,
}

/// Control Plane service.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
    allow_http_upstream: bool,
}

impl ControlPlaneService {
    /// Builds a control plane over the supplied repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            hierarchy,
            allow_http_upstream,
        }
    }

    /// Whether plaintext upstream endpoints may be connected to.
    #[must_use]
    pub fn allow_http_upstream(&self) -> bool {
        self.allow_http_upstream
    }

    // -----------------------------------------------------------------
    // Upstreams
    // -----------------------------------------------------------------

    /// Creates an upstream, deriving (or enforcing) its alias from the endpoints.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] or [`DomainError::Conflict`].
    pub async fn create_upstream(
        &self,
        tenant_id: &str,
        mut upstream: Upstream,
        provided_alias: Option<String>,
    ) -> Result<Upstream, DomainError> {
        Self::validate_upstream(&upstream)?;
        self.validate_plugin_references_exist(tenant_id, &upstream.plugins.items)
            .await?;
        let derived =
            alias::enforce_alias_create(&upstream.server.endpoints, provided_alias.as_deref())?;
        upstream.alias = derived;
        upstream.tenant_id = tenant_id.to_owned();
        upstream.id = Uuid::new_v4();
        upstream.gts_id = crate::domain::gts_helpers::resource_id(
            crate::domain::gts_helpers::TYPE_UPSTREAM,
            &upstream.id,
        );
        upstream.created_at = crate::domain::model::now_rfc3339();
        upstream.updated_at = upstream.created_at.clone();
        self.upstreams.insert(upstream.clone()).await?;
        Ok(upstream)
    }

    /// Reads an upstream by id.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the upstream is not in this tenant.
    pub async fn get_upstream(&self, tenant_id: &str, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("upstream {id} not found")))
    }

    /// Lists upstreams owned by the tenant.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn list_upstreams(&self, tenant_id: &str) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list(tenant_id).await
    }

    /// Replaces an upstream, applying the alias transition matrix.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`], [`DomainError::NotFound`] or
    /// [`DomainError::Conflict`].
    pub async fn replace_upstream(
        &self,
        tenant_id: &str,
        id: Uuid,
        mut upstream: Upstream,
        provided_alias: Option<String>,
    ) -> Result<Upstream, DomainError> {
        let existing = self.get_upstream(tenant_id, id).await?;
        Self::validate_upstream(&upstream)?;
        self.validate_plugin_references_exist(tenant_id, &upstream.plugins.items)
            .await?;
        let existing_derivable = matches!(
            alias::compute_derived_alias(&existing.server.endpoints)?,
            alias::AliasDerivation::Derived(_)
        );
        let alias = alias::enforce_alias_update(
            &existing.alias,
            existing_derivable,
            &upstream.server.endpoints,
            provided_alias.as_deref(),
        )?;
        upstream.id = existing.id;
        upstream.tenant_id = existing.tenant_id;
        upstream.gts_id = existing.gts_id;
        upstream.alias = alias;
        upstream.created_at = existing.created_at;
        upstream.updated_at = crate::domain::model::now_rfc3339();
        self.upstreams.update(upstream.clone()).await?;
        Ok(upstream)
    }

    /// Deletes an upstream and, by cascade, its routes.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the upstream is not in this tenant.
    pub async fn delete_upstream(&self, tenant_id: &str, id: Uuid) -> Result<(), DomainError> {
        self.get_upstream(tenant_id, id).await?;
        self.routes.delete_by_upstream(tenant_id, id).await?;
        self.upstreams.delete(tenant_id, id).await?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Routes
    // -----------------------------------------------------------------

    /// Creates a route.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] or [`DomainError::Conflict`].
    pub async fn create_route(
        &self,
        tenant_id: &str,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        route.match_config.validate()?;
        validate_route_match(&route)?;
        validate_route_cors(&route)?;
        self.validate_plugin_references_exist(tenant_id, &route.plugins.items)
            .await?;
        let upstream = self.get_upstream(tenant_id, route.upstream_id).await?;
        if !upstream.is_http_protocol() {
            return Err(DomainError::Validation(
                "only HTTP upstreams can be bound to an HTTP route".to_owned(),
            ));
        }
        route.tenant_id = tenant_id.to_owned();
        route.id = Uuid::new_v4();
        route.gts_id = crate::domain::gts_helpers::resource_id(
            crate::domain::gts_helpers::TYPE_ROUTE,
            &route.id,
        );
        route.created_at = crate::domain::model::now_rfc3339();
        route.updated_at = route.created_at.clone();
        self.validate_match_uniqueness(tenant_id, &route, None)
            .await?;
        self.routes.insert(route.clone()).await?;
        Ok(route)
    }

    /// Reads a route by id.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the route is not in this tenant.
    pub async fn get_route(&self, tenant_id: &str, id: Uuid) -> Result<Route, DomainError> {
        self.routes
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("route {id} not found")))
    }

    /// Lists routes owned by the tenant.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn list_routes(&self, tenant_id: &str) -> Result<Vec<Route>, DomainError> {
        self.routes.list(tenant_id).await
    }

    /// Replaces a route; `upstream_id` is immutable.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`], [`DomainError::NotFound`] or
    /// [`DomainError::Conflict`].
    pub async fn replace_route(
        &self,
        tenant_id: &str,
        id: Uuid,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        let existing = self.get_route(tenant_id, id).await?;
        route.match_config.validate()?;
        validate_route_match(&route)?;
        validate_route_cors(&route)?;
        self.validate_plugin_references_exist(tenant_id, &route.plugins.items)
            .await?;
        if route.upstream_id != existing.upstream_id {
            return Err(DomainError::Validation(
                "upstream_id is immutable on routes".to_owned(),
            ));
        }
        route.id = existing.id;
        route.tenant_id = existing.tenant_id;
        route.gts_id = existing.gts_id;
        route.created_at = existing.created_at;
        route.updated_at = crate::domain::model::now_rfc3339();
        self.validate_match_uniqueness(tenant_id, &route, Some(existing.id))
            .await?;
        self.routes.update(route.clone()).await?;
        Ok(route)
    }

    /// Deletes a route.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the route is not in this tenant.
    pub async fn delete_route(&self, tenant_id: &str, id: Uuid) -> Result<(), DomainError> {
        self.get_route(tenant_id, id).await?;
        self.routes.delete(tenant_id, id).await?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Plugins
    // -----------------------------------------------------------------

    /// Creates a custom plugin definition (immutable after creation).
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] for an unknown plugin type.
    pub async fn create_plugin(
        &self,
        tenant_id: &str,
        mut plugin: Plugin,
    ) -> Result<Plugin, DomainError> {
        if plugin.name.trim().is_empty() {
            return Err(DomainError::Validation(
                "plugin name must not be empty".to_owned(),
            ));
        }
        if !Plugin::known_type(&plugin.plugin_type) {
            return Err(DomainError::Validation(format!(
                "unknown plugin type `{}` (expected auth, guard or transform)",
                plugin.plugin_type
            )));
        }
        plugin.tenant_id = tenant_id.to_owned();
        plugin.id = Uuid::new_v4();
        plugin.gts_id = format!(
            "gts.cf.core.oagw.{}_plugin.v1~{}",
            plugin.plugin_type, plugin.id
        );
        plugin.created_at = crate::domain::model::now_rfc3339();
        self.plugins.insert(plugin.clone()).await?;
        Ok(plugin)
    }

    /// Reads a plugin definition.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when the plugin is not in this tenant.
    pub async fn get_plugin(&self, tenant_id: &str, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("plugin {id} not found")))
    }

    /// Lists plugins owned by the tenant.
    ///
    /// # Errors
    /// Propagates repository failures.
    pub async fn list_plugins(&self, tenant_id: &str) -> Result<Vec<Plugin>, DomainError> {
        self.plugins.list(tenant_id).await
    }

    /// Deletes a plugin, refusing while any upstream or route still references it.
    ///
    /// # Errors
    /// Returns [`DomainError::PluginInUse`] or [`DomainError::NotFound`].
    pub async fn delete_plugin(&self, tenant_id: &str, id: Uuid) -> Result<(), DomainError> {
        self.get_plugin(tenant_id, id).await?;
        // Both lists are collected so the `409` can name the referencing
        // resources, not just count them (ADR 0001 `referenced_by`).
        let upstreams: Vec<String> = self
            .list_upstreams(tenant_id)
            .await?
            .iter()
            .filter(|u| Self::plugin_bound_to_upstream(u, id))
            .map(|u| u.gts_id.clone())
            .collect();
        let routes: Vec<String> = self
            .list_routes(tenant_id)
            .await?
            .iter()
            .filter(|r| Self::plugin_bound_to_route(r, id))
            .map(|r| r.gts_id.clone())
            .collect();
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(DomainError::PluginInUse {
                upstreams: upstreams.len(),
                routes: routes.len(),
                upstream_ids: upstreams,
                route_ids: routes,
            });
        }
        self.plugins.delete(tenant_id, id).await?;
        Ok(())
    }

    fn plugin_bound_to_upstream(upstream: &Upstream, id: Uuid) -> bool {
        upstream
            .plugins
            .items
            .iter()
            .any(|b| crate::domain::gts_helpers::uuid_from_resource_id(b.plugin_ref()) == Some(id))
            || upstream
                .auth
                .as_ref()
                .and_then(|a| a.plugin_type.as_deref())
                .and_then(crate::domain::gts_helpers::uuid_from_resource_id)
                .is_some_and(|u| u == id)
    }

    fn plugin_bound_to_route(route: &Route, id: Uuid) -> bool {
        route
            .plugins
            .items
            .iter()
            .any(|b| crate::domain::gts_helpers::uuid_from_resource_id(b.plugin_ref()) == Some(id))
    }

    // -----------------------------------------------------------------
    // Proxy resolution
    // -----------------------------------------------------------------

    /// Checks that every custom plugin reference in a binding list names a
    /// plugin stored in the calling tenant.
    ///
    /// Built-ins are identified by their GTS reference and need no lookup; a
    /// reference whose instance part is a UUID addresses a stored custom plugin,
    /// and one that does not exist would bind silently and never run.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] for an unresolvable reference.
    async fn validate_plugin_references_exist(
        &self,
        tenant_id: &str,
        bindings: &[PluginBinding],
    ) -> Result<(), DomainError> {
        for binding in bindings {
            validate_plugin_binding(binding)?;
            let Some(uuid) =
                crate::domain::gts_helpers::uuid_from_resource_id(binding.plugin_ref())
            else {
                continue;
            };
            if self.plugins.get(tenant_id, uuid).await?.is_none() {
                return Err(DomainError::Validation(format!(
                    "plugin `{}` does not exist in this tenant",
                    binding.plugin_ref()
                )));
            }
        }
        Ok(())
    }

    /// Resolves an alias + method + path to an effective configuration.
    ///
    /// One walk (ADR 0006): the tenant chain is walked descendant-first for an
    /// upstream owning the alias (closest match wins), routes for that upstream
    /// are then searched the same way, and the effective config is the merge of
    /// upstream and route with ancestor-enforced limits folded in.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] for an unknown alias and
    /// [`DomainError::RouteNotFound`] when no route matches.
    pub async fn resolve_proxy_target(
        &self,
        tenant_id: &str,
        alias: &str,
        method: ProxyMethod,
        path: &str,
    ) -> Result<ResolvedTarget, DomainError> {
        let normalized = alias::normalize_alias(alias);
        if normalized.is_empty() {
            return Err(DomainError::RouteNotFound(
                "proxy path must name an upstream alias".to_owned(),
            ));
        }

        let chain = self.hierarchy.chain(tenant_id).await;
        let mut selected: Option<Upstream> = None;
        for tenant in &chain {
            if let Some(found) = self.upstreams.get_by_alias(tenant, &normalized).await? {
                selected = Some(found);
                break;
            }
        }
        let upstream = selected.ok_or_else(|| {
            DomainError::NotFound(format!(
                "no upstream is registered for alias `{normalized}`"
            ))
        })?;

        if !upstream.enabled {
            return Err(DomainError::LinkUnavailable(
                "upstream is disabled".to_owned(),
            ));
        }

        // Route search across the chain, descendant-first, for the selected upstream.
        let mut matched: Option<(Route, String)> = None;
        for tenant in &chain {
            let routes = self.routes.list_by_upstream(tenant, upstream.id).await?;
            if let Some(found) = best_matching_route(&routes, method, path) {
                matched = Some(found);
                break;
            }
        }
        let (route, path_remainder) = matched.ok_or_else(|| {
            DomainError::RouteNotFound(format!("no route matches {} {path}", method.as_str()))
        })?;

        // The selected upstream's own limit always governs traffic it serves;
        // ancestor limits bind across shadowing only when they are `enforce`.
        let mut enforced_rates: Vec<RateLimitConfig> = Vec::new();
        if let Some(rl) = upstream.rate_limit.as_ref() {
            enforced_rates.push(rl.clone());
        }
        for tenant in &chain {
            if let Some(ancestor) = self.upstreams.get_by_alias(tenant, &normalized).await? {
                if ancestor.id == upstream.id {
                    continue;
                }
                if let Some(rl) = ancestor
                    .rate_limit
                    .as_ref()
                    .filter(|r| r.sharing.is_enforce())
                {
                    enforced_rates.push(rl.clone());
                }
            }
        }
        if let Some(rl) = route.rate_limit.as_ref() {
            enforced_rates.push(rl.clone());
        }
        let rate_limit = strictest(&enforced_rates);

        let mut plugins = upstream.plugins.items.clone();
        plugins.extend(route.plugins.items.iter().cloned());

        let owning_tenant = upstream.tenant_id.clone();
        // A route-level CORS policy overrides the upstream's; otherwise the
        // upstream's governs (DESIGN §"Config Layering": Upstream < Route).
        let cors = route.cors.clone().or_else(|| upstream.cors.clone());
        Ok(ResolvedTarget {
            upstream,
            route,
            path_remainder,
            plugins,
            rate_limit,
            cors,
            owning_tenant,
        })
    }

    // -----------------------------------------------------------------
    // Validation helpers
    // -----------------------------------------------------------------

    async fn validate_match_uniqueness(
        &self,
        tenant_id: &str,
        route: &Route,
        excluding: Option<Uuid>,
    ) -> Result<(), DomainError> {
        let Some(http) = route.match_config.http.as_ref() else {
            return Ok(());
        };
        let siblings = self
            .routes
            .list_by_upstream(tenant_id, route.upstream_id)
            .await?;
        for other in siblings {
            if Some(other.id) == excluding || !other.enabled {
                continue;
            }
            let Some(o) = other.match_config.http.as_ref() else {
                continue;
            };
            // Two enabled routes may share a path only when their priority
            // separates them (DESIGN §"Data Constraints").
            if o.path == http.path
                && other.priority == route.priority
                && o.methods.iter().any(|m| http.methods.contains(m))
            {
                return Err(DomainError::Conflict(format!(
                    "route with the same path `{}`, priority {} and method is already registered \
                     for this upstream",
                    http.path, route.priority
                )));
            }
        }
        Ok(())
    }

    fn validate_upstream(upstream: &Upstream) -> Result<(), DomainError> {
        if upstream.server.endpoints.is_empty() {
            return Err(DomainError::Validation(
                "upstream requires at least one server endpoint".to_owned(),
            ));
        }
        for endpoint in &upstream.server.endpoints {
            alias::validate_host(&endpoint.host)?;
        }
        let scheme = upstream.server.endpoints[0].scheme;
        let port = upstream.server.endpoints[0].effective_port();
        if upstream
            .server
            .endpoints
            .iter()
            .any(|e| e.scheme != scheme || e.effective_port() != port)
        {
            return Err(DomainError::Validation(
                "all endpoints of an upstream must share the same scheme and port".to_owned(),
            ));
        }
        if upstream.protocol != crate::domain::gts_helpers::PROTOCOL_HTTP
            && upstream.protocol != crate::domain::gts_helpers::PROTOCOL_GRPC
        {
            return Err(DomainError::Validation(format!(
                "unknown upstream protocol `{}`",
                upstream.protocol
            )));
        }
        if let Some(auth) = upstream.auth.as_ref()
            && let Some(t) = auth.plugin_type.as_deref()
        {
            validate_auth_plugin_ref(t)?;
        }
        for binding in &upstream.plugins.items {
            validate_plugin_binding(binding)?;
        }
        for tag in &upstream.tags {
            crate::domain::model::validate_tag(tag)?;
        }
        if let Some(cors) = upstream.cors.as_ref() {
            validate_cors(cors)?;
        }
        Ok(())
    }
}

/// A proxy method, decoupled from the management-facing `HttpMethod` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `PATCH`.
    Patch,
    /// Any other method (`HEAD`, `OPTIONS`, ...).
    Other,
}

impl ProxyMethod {
    /// Parses an HTTP method token.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_uppercase().as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PUT" => Self::Put,
            "DELETE" => Self::Delete,
            "PATCH" => Self::Patch,
            _ => Self::Other,
        }
    }

    /// The method as an HTTP token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Other => "OTHER",
        }
    }
}

impl PartialEq<crate::domain::model::HttpMethod> for ProxyMethod {
    fn eq(&self, other: &crate::domain::model::HttpMethod) -> bool {
        matches!(
            (self, other),
            (Self::Get, crate::domain::model::HttpMethod::Get)
                | (Self::Post, crate::domain::model::HttpMethod::Post)
                | (Self::Put, crate::domain::model::HttpMethod::Put)
                | (Self::Delete, crate::domain::model::HttpMethod::Delete)
                | (Self::Patch, crate::domain::model::HttpMethod::Patch)
        )
    }
}

impl crate::domain::model::SharingMode {
    /// `true` for [`SharingMode::Enforce`](crate::domain::model::SharingMode::Enforce).
    #[must_use]
    pub fn is_enforce(self) -> bool {
        matches!(self, crate::domain::model::SharingMode::Enforce)
    }
}

/// Selects the route serving `path`, plus the part of `path` its prefix does not
/// cover (the "path suffix" of DESIGN §3.2).
///
/// Longest prefix wins; disabled routes and gRPC-only matches are skipped. A
/// route whose method allowlist admits the request is preferred over one that
/// does not, so sibling routes can split a path by verb. When nothing admits the
/// method the best path match is still returned: the method rule is a guard rule
/// (DESIGN §"Guard Rules") and is reported as a validation error by the data
/// plane, not as a missing route.
#[must_use]
pub fn best_matching_route(
    routes: &[Route],
    method: ProxyMethod,
    path: &str,
) -> Option<(Route, String)> {
    // `serving` holds the deepest prefix that admits the method, `path_only`
    // the deepest prefix that matches at all. Equal depths are broken by the
    // lower `priority` value (DESIGN §"Find Matching Route for Request").
    let mut serving: Option<(usize, i64, Route, String)> = None;
    let mut path_only: Option<(usize, i64, Route, String)> = None;
    for route in routes {
        if !route.enabled {
            continue;
        }
        let Some(http) = route.match_config.http.as_ref() else {
            continue;
        };
        let Some(remainder) = strip_prefix_path(path, &http.path) else {
            continue;
        };
        let depth = http.path.trim_end_matches('/').matches('/').count();
        let takes = |candidate: &Option<(usize, i64, Route, String)>| {
            candidate.as_ref().is_none_or(|(d, priority, _, _)| {
                depth > *d || (depth == *d && route.priority < *priority)
            })
        };
        if http.methods.iter().any(|m| method == *m) && takes(&serving) {
            serving = Some((depth, route.priority, route.clone(), remainder.clone()));
        }
        if takes(&path_only) {
            path_only = Some((depth, route.priority, route.clone(), remainder));
        }
    }
    serving
        .or(path_only)
        .map(|(_, _, route, remainder)| (route, remainder))
}

/// Splits `path` into the part `prefix` matches and the remainder after it.
///
/// The root prefix matches everything; otherwise the prefix must end on a
/// segment boundary.
#[must_use]
fn strip_prefix_path(path: &str, prefix: &str) -> Option<String> {
    let trimmed = prefix.trim_end_matches('/');
    if trimmed.is_empty() {
        return Some(path.to_owned());
    }
    let candidate = path.trim_end_matches('/');
    if candidate == trimmed {
        return Some(String::new());
    }
    candidate
        .strip_prefix(trimmed)
        .filter(|rest| rest.starts_with('/'))
        .map(str::to_owned)
}

/// The strictest (minimum-throughput) rate limit of a set.
#[must_use]
pub fn strictest(limits: &[RateLimitConfig]) -> Option<RateLimitConfig> {
    limits
        .iter()
        .min_by(|a, b| {
            a.capacity()
                .cmp(&b.capacity())
                .then_with(|| b.sustained.rate.cmp(&a.sustained.rate))
        })
        .cloned()
}

fn validate_route_match(route: &Route) -> Result<(), DomainError> {
    if let Some(http) = route.match_config.http.as_ref() {
        if http.methods.is_empty() {
            return Err(DomainError::Validation(
                "route must declare at least one HTTP method".to_owned(),
            ));
        }
        if http.path.is_empty() || !http.path.starts_with('/') {
            return Err(DomainError::Validation(format!(
                "route path `{}` must start with `/`",
                http.path
            )));
        }
        return Ok(());
    }
    // A gRPC match is storable per the route schema; no HTTP code path matches it.
    Ok(())
}

/// Validates a route's own CORS policy, when it declares one.
///
/// # Errors
/// Returns [`DomainError::Validation`] for a malformed CORS block.
fn validate_route_cors(route: &Route) -> Result<(), DomainError> {
    route.cors.as_ref().map_or(Ok(()), |cors| {
        validate_cors(cors).map_err(|mut e| {
            // Name the layer that failed: the same block is legal on an
            // upstream, so the message has to say which one was rejected.
            if let DomainError::Validation(detail) = &mut e {
                *detail = format!("route cors: {detail}");
            }
            e
        })
    })
}

/// Rejects auth plugin types that have no backing implementation.
///
/// `basic` and `bearer` are catalog identifiers only.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an unknown identifier.
pub fn validate_auth_plugin_ref(plugin_ref: &str) -> Result<(), DomainError> {
    use crate::domain::gts_helpers as g;
    match plugin_ref {
        g::AUTH_NOOP | g::AUTH_APIKEY | g::AUTH_OAUTH2_CC | g::AUTH_OAUTH2_CC_BASIC => Ok(()),
        g::AUTH_BASIC | g::AUTH_BEARER => Err(DomainError::Validation(format!(
            "unknown auth plugin `{plugin_ref}`: it is a catalog identifier with no backing \
             implementation"
        ))),
        _ => Err(DomainError::Validation(format!(
            "unknown auth plugin `{plugin_ref}`"
        ))),
    }
}

/// Validates a `plugins.items[]` binding, resolving it against the built-in
/// registries and the catalog-only identifiers.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an unresolvable reference.
pub fn validate_plugin_binding(binding: &PluginBinding) -> Result<(), DomainError> {
    use crate::domain::gts_helpers as g;

    let reference = binding.plugin_ref();
    // A UUID instance part addresses a stored custom plugin; those are validated
    // by the management service when the upstream is stored.
    if crate::domain::gts_helpers::uuid_from_resource_id(reference).is_some() {
        return Ok(());
    }
    #[allow(clippy::match_same_arms)]
    match reference {
        g::GUARD_REQUIRED_HEADERS | g::TRANSFORM_REQUEST_ID => Ok(()),
        g::GUARD_TIMEOUT | g::GUARD_CORS | g::TRANSFORM_LOGGING | g::TRANSFORM_METRICS => {
            Err(DomainError::Validation(format!(
                "plugin `{reference}` is a catalog identifier and cannot be bound through \
                 plugins.items"
            )))
        }
        other => Err(DomainError::Validation(format!("unknown plugin `{other}`"))),
    }
}

/// Validates a CORS block, including the wildcard + credentials rejection.
///
/// # Errors
/// Returns [`DomainError::Validation`] for a bad origin or method, or for a
/// wildcard origin combined with credentials.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), DomainError> {
    if !cors.enabled {
        return Ok(());
    }
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::Validation(
            "cors.allow_credentials cannot be combined with a wildcard origin".to_owned(),
        ));
    }
    for origin in &cors.allowed_origins {
        crate::domain::model::validate_origin(origin)?;
    }
    for method in &cors.allowed_methods {
        if !matches!(
            method.to_ascii_uppercase().as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ) {
            return Err(DomainError::Validation(format!(
                "invalid cors method `{method}`"
            )));
        }
    }
    Ok(())
}

/// Validates that an endpoint's scheme may be connected to at proxy time.
///
/// # Errors
/// Returns [`DomainError::LinkUnavailable`] when a plaintext `http` endpoint is
/// not admitted by the configuration.
pub fn check_scheme_admission(
    scheme: EndpointScheme,
    allow_http_upstream: bool,
) -> Result<(), DomainError> {
    if scheme == EndpointScheme::Http && !allow_http_upstream {
        return Err(DomainError::LinkUnavailable(
            "plaintext http upstreams are disabled by configuration".to_owned(),
        ));
    }
    Ok(())
}
