//! Control Plane — configuration ownership and resolution.
//!
//! Everything the management API writes goes through [`ControlPlaneService`],
//! which is also what the Data Plane asks for a resolved proxy target. Tenant
//! scoping is absolute here: ancestor resources are invisible (404) to the
//! management API and only reachable through
//! [`ControlPlaneService::resolve_proxy_target`].

use std::sync::Arc;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts;
use crate::domain::input::{PluginInput, RouteInput, UpstreamInput};
use crate::domain::merge::{EffectiveConfig, effective_config};
use crate::domain::model::{
    CorsConfig, Endpoint, MatchConfig, PluginBinding, PluginDef, PluginKind, PluginsConfig,
    Protocol, RateLimitConfig, Route, Scheme, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::tenant::TenantDirectory;

/// Named plugin identifiers that may be bound through `plugins.items[]`.
///
/// Timeout, CORS, logging and metrics are core Data Plane behaviour; their GTS
/// identifiers exist for types-registry cataloging only.
const BINDABLE_NAMED_PLUGINS: &[&str] = &[
    gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
    gts::REQUEST_ID_TRANSFORM_PLUGIN_ID,
];

/// Auth plugin identifiers the catalog knows about. `basic` and `bearer` are
/// accepted here but have no backing implementation, so they fail at proxy
/// time with `unknown auth plugin`.
const CATALOG_AUTH_PLUGINS: &[&str] = &[
    gts::NOOP_AUTH_PLUGIN_ID,
    gts::APIKEY_AUTH_PLUGIN_ID,
    gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID,
    gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    gts::BASIC_AUTH_PLUGIN_ID,
    gts::BEARER_AUTH_PLUGIN_ID,
];

/// HTTP methods a route may match on (`schemas/route.v1.schema.json`).
const ALLOWED_ROUTE_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH"];

/// A resolved proxy target: which upstream, which route, and the configuration
/// the two produce once the tenant hierarchy is folded in.
#[derive(Debug, Clone)]
pub struct ProxyTarget {
    pub upstream: Upstream,
    /// Upstreams sharing the alias further up the chain, ordered parent → root.
    pub ancestors: Vec<Upstream>,
    pub route: Route,
    pub effective: EffectiveConfig,
}

/// Control Plane service.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    tenants: Arc<dyn TenantDirectory>,
}

impl ControlPlaneService {
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        tenants: Arc<dyn TenantDirectory>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            tenants,
        }
    }

    // -- Upstreams ---------------------------------------------------------

    /// Create an upstream for the calling tenant.
    ///
    /// # Errors
    ///
    /// `400` when the payload fails validation, `409` when the alias is taken.
    pub fn create_upstream(&self, tenant_id: Uuid, input: UpstreamInput) -> OagwResult<Upstream> {
        let endpoints = self.validate_endpoints(&input.server.endpoints, input.protocol)?;
        let alias = alias::resolve_alias_for_create(&endpoints, input.alias.as_deref())?;
        let upstream = self.assemble_upstream(Uuid::new_v4(), tenant_id, alias, endpoints, input)?;
        self.upstreams.insert(upstream)
    }

    /// Full replacement of an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `400` on validation failure (including any change that would move the
    /// alias), `404` when the upstream is not the tenant's.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        input: UpstreamInput,
    ) -> OagwResult<Upstream> {
        let Some(existing) = self.upstreams.get(tenant_id, id) else {
            return Err(OagwError::not_found("upstream not found"));
        };
        let endpoints = self.validate_endpoints(&input.server.endpoints, input.protocol)?;
        let alias =
            alias::enforce_alias_update(&existing.alias, &endpoints, input.alias.as_deref())?;
        let upstream = self.assemble_upstream(id, tenant_id, alias, endpoints, input)?;
        self.upstreams.replace(upstream)
    }

    /// Fetch an upstream owned by the calling tenant.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.upstreams.get(tenant_id, id)
    }

    /// Every upstream owned by the calling tenant.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.upstreams.list(tenant_id)
    }

    /// Delete an upstream and cascade to its routes.
    ///
    /// # Errors
    ///
    /// `404` when the upstream is not the tenant's.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<()> {
        if !self.upstreams.delete(tenant_id, id) {
            return Err(OagwError::not_found("upstream not found"));
        }
        self.routes.delete_by_upstream(id);
        Ok(())
    }

    // -- Routes ------------------------------------------------------------

    /// Create a route under an upstream owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `400` on validation failure (including an unknown `upstream_id`), `409`
    /// when an equivalent match rule already exists.
    pub fn create_route(&self, tenant_id: Uuid, input: RouteInput) -> OagwResult<Route> {
        let Some(upstream_id) = input.upstream_id else {
            return Err(OagwError::validation("upstream_id is required"));
        };
        let upstream = self.require_own_upstream(tenant_id, upstream_id)?;
        let route = self.assemble_route(Uuid::new_v4(), tenant_id, upstream_id, &upstream, input)?;
        self.routes.insert(route)
    }

    /// Full replacement of a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `400` on validation failure or an attempt to move the route to another
    /// upstream, `404` when the route is not the tenant's, `409` on a
    /// duplicate match rule.
    pub fn replace_route(&self, tenant_id: Uuid, id: Uuid, input: RouteInput) -> OagwResult<Route> {
        let Some(existing) = self.routes.get(tenant_id, id) else {
            return Err(OagwError::not_found("route not found"));
        };
        if let Some(requested) = input.upstream_id
            && requested != existing.upstream_id
        {
            return Err(OagwError::validation(
                "upstream_id is immutable; delete and re-create the route to move it",
            ));
        }
        let upstream = self.require_own_upstream(tenant_id, existing.upstream_id)?;
        let route =
            self.assemble_route(id, tenant_id, existing.upstream_id, &upstream, input)?;
        self.routes.replace(route)
    }

    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.routes.get(tenant_id, id)
    }

    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.routes.list(tenant_id)
    }

    /// Delete a route owned by the calling tenant.
    ///
    /// # Errors
    ///
    /// `404` when the route is not the tenant's.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<()> {
        if self.routes.delete(tenant_id, id) {
            Ok(())
        } else {
            Err(OagwError::not_found("route not found"))
        }
    }

    // -- Plugins -----------------------------------------------------------

    /// Register a custom plugin definition. Definitions are immutable.
    ///
    /// # Errors
    ///
    /// `400` on validation failure, `409` when the name is taken.
    pub fn create_plugin(&self, tenant_id: Uuid, input: PluginInput) -> OagwResult<PluginDef> {
        let name = input.name.trim().to_owned();
        if name.is_empty() {
            return Err(OagwError::validation("plugin name must not be empty"));
        }
        if input.source_code.trim().is_empty() {
            return Err(OagwError::validation("plugin source_code must not be empty"));
        }
        let plugin = PluginDef {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: input.plugin_type,
            name,
            description: input.description,
            phases: default_phases(input.plugin_type, input.phases),
            config_schema: input.config_schema,
            source_code: input.source_code,
            last_used_at: None,
            gc_eligible_at: None,
        };
        self.plugins.insert(plugin)
    }

    #[must_use]
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginDef> {
        self.plugins.get(tenant_id, id)
    }

    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<PluginDef> {
        self.plugins.list(tenant_id)
    }

    /// Delete an unlinked plugin.
    ///
    /// # Errors
    ///
    /// `404` when the plugin is not the tenant's, `409` when it is still
    /// referenced by an upstream or route.
    pub fn delete_plugin(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        referenced_by: (Vec<Uuid>, Vec<Uuid>),
    ) -> OagwResult<()> {
        let Some(plugin) = self.plugins.get(tenant_id, id) else {
            return Err(OagwError::not_found("plugin not found"));
        };
        let (upstreams, routes) = referenced_by;
        if !upstreams.is_empty() || !routes.is_empty() {
            return Err(OagwError::new(
                ErrorKind::PluginInUse,
                format!(
                    "Plugin is referenced by {} upstream(s) and {} route(s)",
                    upstreams.len(),
                    routes.len()
                ),
            )
            .with("plugin_id", plugin.gts_id())
            .with(
                "referenced_by",
                serde_json::json!({
                    "upstreams": upstreams
                        .iter()
                        .map(|id| format!("{}{id}", gts::UPSTREAM_BASE))
                        .collect::<Vec<_>>(),
                    "routes": routes
                        .iter()
                        .map(|id| format!("{}{id}", gts::ROUTE_BASE))
                        .collect::<Vec<_>>(),
                }),
            ));
        }
        if self.plugins.delete(tenant_id, id) {
            Ok(())
        } else {
            Err(OagwError::not_found("plugin not found"))
        }
    }

    // -- Proxy resolution --------------------------------------------------

    /// Resolve `alias` for `ctx`'s tenant and pick the route that matches
    /// `method` + `path_suffix`.
    ///
    /// Walks the tenant chain from descendant to root: the closest upstream
    /// under the alias wins, and enforced ancestor constraints still apply.
    ///
    /// # Errors
    ///
    /// `404` when no upstream or no route matches, `503` when the upstream (or
    /// an ancestor's upstream under the same alias) is disabled.
    pub async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        alias_raw: &str,
        method: &str,
        path_suffix: &str,
    ) -> OagwResult<ProxyTarget> {
        let alias = alias::normalize(alias_raw);
        let chain = self
            .tenants
            .ancestor_chain(ctx, ctx.subject_tenant_id())
            .await;

        let candidates: Vec<Upstream> = chain
            .iter()
            .filter_map(|tenant| self.upstreams.find_by_alias(*tenant, &alias))
            .collect();

        let Some((selected, ancestors)) = candidates.split_first() else {
            return Err(
                OagwError::not_found(format!("no upstream is configured for alias '{alias}'"))
                    .with("alias", alias.clone()),
            );
        };

        // An ancestor that disables the alias disables it for every descendant.
        if let Some(disabled) = candidates.iter().find(|u| !u.enabled) {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream '{alias}' is disabled"),
            )
            .with("alias", alias.clone())
            .with("upstream_id", disabled.gts_id()));
        }

        let route = candidates
            .iter()
            .find_map(|upstream| self.match_route(upstream, method, path_suffix))
            .ok_or_else(|| {
                OagwError::not_found(format!(
                    "no route on upstream '{alias}' matches {method} {path_suffix}"
                ))
                .with("alias", alias.clone())
                .with("upstream_id", selected.gts_id())
                .with("path", path_suffix.to_owned())
            })?;

        let effective = effective_config(selected, ancestors, Some(&route));
        Ok(ProxyTarget {
            upstream: selected.clone(),
            ancestors: ancestors.to_vec(),
            route,
            effective,
        })
    }

    /// Best enabled route on `upstream` for `method` + `path_suffix`:
    /// longest matching path prefix, ties broken by higher priority.
    fn match_route(&self, upstream: &Upstream, method: &str, path_suffix: &str) -> Option<Route> {
        let inbound = normalize_path(path_suffix);
        let mut best: Option<(usize, i32, Route)> = None;

        for route in self.routes.list_by_upstream(upstream.id) {
            if !route.enabled {
                continue;
            }
            let Some(http) = route.http() else {
                // gRPC routes are stored but not yet routable.
                continue;
            };
            if !http.allows_method(method) {
                continue;
            }
            let route_path = normalize_path(&http.path);
            if !path_prefix_matches(&route_path, &inbound) {
                continue;
            }
            let score = route_path.len();
            let better = best
                .as_ref()
                .is_none_or(|(len, prio, _)| score > *len || (score == *len && route.priority > *prio));
            if better {
                best = Some((score, route.priority, route));
            }
        }
        best.map(|(_, _, route)| route)
    }

    // -- Validation helpers ------------------------------------------------

    fn require_own_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> OagwResult<Upstream> {
        self.upstreams.get(tenant_id, upstream_id).ok_or_else(|| {
            OagwError::validation(format!(
                "upstream '{upstream_id}' does not exist for this tenant"
            ))
            .with("upstream_id", upstream_id.to_string())
        })
    }

    /// Endpoint pool rules: at least one endpoint, valid RFC 1123 hosts, and a
    /// homogeneous `(scheme, port)` across the pool.
    fn validate_endpoints(
        &self,
        endpoints: &[Endpoint],
        protocol: Protocol,
    ) -> OagwResult<Vec<Endpoint>> {
        let Some(first) = endpoints.first() else {
            return Err(OagwError::validation(
                "server.endpoints must contain at least one endpoint",
            ));
        };
        let mut normalized = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            if endpoint.scheme != first.scheme {
                return Err(OagwError::validation(
                    "all endpoints in a pool must share the same scheme",
                ));
            }
            if endpoint.port != first.port {
                return Err(OagwError::validation(
                    "all endpoints in a pool must share the same port",
                ));
            }
            if endpoint.port == 0 {
                return Err(OagwError::validation("endpoint port must be 1-65535"));
            }
            normalized.push(Endpoint {
                scheme: endpoint.scheme,
                host: alias::validate_host(&endpoint.host)?,
                port: endpoint.port,
            });
        }

        match (protocol, first.scheme) {
            (Protocol::Grpc, Scheme::Grpc) | (Protocol::Http, _) => {}
            (Protocol::Grpc, _) => {
                return Err(OagwError::validation(
                    "a gRPC upstream requires endpoints with the 'grpc' scheme",
                ));
            }
        }
        if protocol == Protocol::Http && first.scheme == Scheme::Grpc {
            return Err(OagwError::validation(
                "the 'grpc' scheme requires the gRPC protocol",
            ));
        }

        Ok(normalized)
    }

    fn assemble_upstream(
        &self,
        id: Uuid,
        tenant_id: Uuid,
        alias: String,
        endpoints: Vec<Endpoint>,
        input: UpstreamInput,
    ) -> OagwResult<Upstream> {
        validate_tags(&input.tags)?;
        if let Some(auth) = input.auth.as_ref()
            && let Some(plugin_type) = auth.plugin_type.as_deref()
        {
            self.validate_auth_ref(tenant_id, plugin_type)?;
        }
        let plugins = self.validate_plugins(tenant_id, input.plugins)?;
        if let Some(limit) = input.rate_limit.as_ref() {
            validate_rate_limit(limit)?;
        }
        if let Some(cors) = input.cors.as_ref() {
            validate_cors(cors)?;
        }

        Ok(Upstream {
            id,
            tenant_id,
            alias,
            enabled: input.enabled,
            protocol: input.protocol,
            server: crate::domain::model::ServerConfig { endpoints },
            auth: input.auth,
            headers: input.headers,
            plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
            tags: normalize_tags(input.tags),
        })
    }

    fn assemble_route(
        &self,
        id: Uuid,
        tenant_id: Uuid,
        upstream_id: Uuid,
        upstream: &Upstream,
        input: RouteInput,
    ) -> OagwResult<Route> {
        validate_tags(&input.tags)?;
        let match_config = validate_match(&input.r#match, upstream.protocol)?;
        let plugins = self.validate_plugins(tenant_id, input.plugins)?;
        if let Some(limit) = input.rate_limit.as_ref() {
            validate_rate_limit(limit)?;
        }
        if let Some(cors) = input.cors.as_ref() {
            validate_cors(cors)?;
        }

        Ok(Route {
            id,
            tenant_id,
            upstream_id,
            enabled: input.enabled,
            priority: input.priority,
            r#match: match_config,
            plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
            tags: normalize_tags(input.tags),
        })
    }

    /// An auth reference must be a catalogued named plugin or a UUID-backed
    /// auth plugin owned by the tenant.
    fn validate_auth_ref(&self, tenant_id: Uuid, plugin_type: &str) -> OagwResult<()> {
        if CATALOG_AUTH_PLUGINS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(plugin_type))
        {
            return Ok(());
        }
        if let Some(uuid) = gts::instance_uuid(plugin_type)
            && gts::matches_plugin_base(plugin_type, gts::AUTH_PLUGIN_BASE)
        {
            return match self.plugins.get(tenant_id, uuid) {
                Some(plugin) if plugin.plugin_type == PluginKind::Auth => Ok(()),
                Some(_) => Err(OagwError::validation(format!(
                    "plugin '{plugin_type}' is not an auth plugin"
                ))),
                None => Err(OagwError::validation(format!(
                    "unknown auth plugin: {plugin_type}"
                ))),
            };
        }
        Err(OagwError::validation(format!(
            "unknown auth plugin: {plugin_type}"
        )))
    }

    /// Guard/transform bindings must name a bindable built-in or a UUID-backed
    /// custom plugin owned by the tenant.
    fn validate_plugins(
        &self,
        tenant_id: Uuid,
        plugins: Option<PluginsConfig>,
    ) -> OagwResult<Option<PluginsConfig>> {
        let Some(mut plugins) = plugins else {
            return Ok(None);
        };
        let mut validated = Vec::with_capacity(plugins.items.len());
        for item in plugins.items {
            let item = item.with_derived_uuid();
            self.validate_plugin_binding(tenant_id, &item)?;
            validated.push(item);
        }
        plugins.items = validated;
        Ok(Some(plugins))
    }

    fn validate_plugin_binding(&self, tenant_id: Uuid, item: &PluginBinding) -> OagwResult<()> {
        if BINDABLE_NAMED_PLUGINS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&item.plugin_ref))
        {
            return Ok(());
        }
        if let Some(uuid) = item.plugin_uuid {
            return if self.plugins.get(tenant_id, uuid).is_some() {
                Ok(())
            } else {
                Err(OagwError::validation(format!(
                    "unknown plugin: {}",
                    item.plugin_ref
                )))
            };
        }
        Err(OagwError::validation(format!(
            "plugin '{}' cannot be bound through plugins.items",
            item.plugin_ref
        )))
    }
}

fn default_phases(kind: PluginKind, requested: Vec<crate::domain::model::PluginPhase>) -> Vec<crate::domain::model::PluginPhase> {
    use crate::domain::model::PluginPhase;
    if !requested.is_empty() {
        return requested;
    }
    match kind {
        PluginKind::Auth | PluginKind::Guard => vec![PluginPhase::OnRequest],
        PluginKind::Transform => vec![PluginPhase::OnRequest, PluginPhase::OnResponse],
    }
}

/// Ensure a path starts with `/` and carries no trailing slash beyond the root.
#[must_use]
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    let with_root = if trimmed.starts_with('/') {
        trimmed.to_owned()
    } else {
        format!("/{trimmed}")
    };
    let stripped = with_root.trim_end_matches('/');
    if stripped.is_empty() {
        "/".to_owned()
    } else {
        stripped.to_owned()
    }
}

/// Segment-aware prefix test: `/v1` matches `/v1/chat` but not `/v11`.
#[must_use]
pub fn path_prefix_matches(route_path: &str, inbound: &str) -> bool {
    if route_path == "/" {
        return true;
    }
    if inbound == route_path {
        return true;
    }
    inbound
        .strip_prefix(route_path)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for tag in tags {
        let tag = tag.trim().to_ascii_lowercase();
        if !tag.is_empty() && !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

fn validate_tags(tags: &[String]) -> OagwResult<()> {
    for tag in tags {
        let normalized = tag.trim();
        if normalized.is_empty()
            || !normalized
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(OagwError::validation(format!(
                "tag '{tag}' must match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(())
}

fn validate_rate_limit(limit: &RateLimitConfig) -> OagwResult<()> {
    if limit.sustained.rate == 0 {
        return Err(OagwError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if limit.burst.capacity == Some(0) {
        return Err(OagwError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if limit.cost == 0 {
        return Err(OagwError::validation("rate_limit.cost must be at least 1"));
    }
    Ok(())
}

fn validate_cors(cors: &CorsConfig) -> OagwResult<()> {
    if cors.allow_credentials && cors.has_wildcard_origin() {
        return Err(OagwError::validation(
            "cannot use allow_credentials with wildcard origin",
        ));
    }
    for method in &cors.allowed_methods {
        if !matches!(
            method.to_ascii_uppercase().as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
        ) {
            return Err(OagwError::validation(format!(
                "cors.allowed_methods contains an unsupported method: {method}"
            )));
        }
    }
    Ok(())
}

fn validate_match(config: &MatchConfig, protocol: Protocol) -> OagwResult<MatchConfig> {
    match (&config.http, &config.grpc) {
        (Some(_), Some(_)) => Err(OagwError::validation(
            "match must contain exactly one of 'http' or 'grpc'",
        )),
        (None, None) => Err(OagwError::validation(
            "match must contain exactly one of 'http' or 'grpc'",
        )),
        (Some(http), None) => {
            if protocol != Protocol::Http {
                return Err(OagwError::validation(
                    "an HTTP match requires an upstream with the HTTP protocol",
                ));
            }
            if http.methods.is_empty() {
                return Err(OagwError::validation(
                    "match.http.methods must contain at least one method",
                ));
            }
            for method in &http.methods {
                if !ALLOWED_ROUTE_METHODS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(method))
                {
                    return Err(OagwError::validation(format!(
                        "match.http.methods contains an unsupported method: {method}"
                    )));
                }
            }
            if http.path.trim().is_empty() {
                return Err(OagwError::validation("match.http.path must not be empty"));
            }
            let mut normalized = http.clone();
            normalized.methods = http
                .methods
                .iter()
                .map(|m| m.to_ascii_uppercase())
                .collect();
            normalized.path = normalize_path(&http.path);
            Ok(MatchConfig {
                http: Some(normalized),
                grpc: None,
            })
        }
        (None, Some(grpc)) => {
            if protocol != Protocol::Grpc {
                return Err(OagwError::validation(
                    "a gRPC match requires an upstream with the gRPC protocol",
                ));
            }
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(OagwError::validation(
                    "match.grpc.service and match.grpc.method must not be empty",
                ));
            }
            Ok(config.clone())
        }
    }
}

#[cfg(test)]
#[path = "services_tests.rs"]
mod tests;
