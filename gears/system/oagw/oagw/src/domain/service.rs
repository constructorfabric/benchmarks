//! Control-plane service: CRUD, validation and alias enforcement.
//!
//! This is the domain layer's application service. It owns no transport types
//! and no storage — only the repository traits.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::DomainError;
use crate::domain::model::gts;
use crate::domain::model::{
    Endpoint, HeadersConfig, HttpMethod, MatchConfig, Plugin, PluginBinding, RateLimitConfig,
    Route, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Repositories the control plane operates on.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
    allow_http_upstream: bool,
}

impl ControlPlaneService {
    /// Assemble a service over the given repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        plugins: Arc<dyn PluginRepository>,
        allow_http_upstream: bool,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            allow_http_upstream,
        }
    }

    /// The upstream repository, for proxy-time alias resolution.
    #[must_use]
    pub fn upstreams(&self) -> &Arc<dyn UpstreamRepository> {
        &self.upstreams
    }

    /// The route repository, for proxy-time route matching.
    #[must_use]
    pub fn routes(&self) -> &Arc<dyn RouteRepository> {
        &self.routes
    }

    /// The plugin repository, for plugin-in-use checks.
    #[must_use]
    pub fn plugins(&self) -> &Arc<dyn PluginRepository> {
        &self.plugins
    }

    /// Create an upstream after full validation.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails or the alias is taken.
    pub async fn create_upstream(
        &self,
        tenant_id: Uuid,
        user_alias: Option<String>,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        normalize_and_validate(&mut upstream, self.allow_http_upstream)?;
        let derived = alias::compute_derived_alias(&upstream.server.endpoints);
        let resolved =
            alias::enforce_alias_update_with(derived, user_alias.as_deref(), "upstream create")?;
        upstream.alias = resolved;
        upstream.id = Uuid::new_v4();
        upstream.tenant_id = tenant_id;
        self.validate_upstream_bindings(tenant_id, &upstream)
            .await?;
        self.upstreams.insert(upstream.clone()).await?;
        Ok(upstream)
    }

    /// Replace an upstream, enforcing alias immutability.
    ///
    /// # Errors
    /// Returns [`DomainError`] when the upstream does not exist, validation
    /// fails, or the alias would change.
    pub async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        user_alias: Option<String>,
        mut next: Upstream,
    ) -> Result<Upstream, DomainError> {
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("upstream {id}")))?;
        normalize_and_validate(&mut next, self.allow_http_upstream)?;
        let resolved = alias::enforce_alias_update_derived(
            &existing,
            &next.server.endpoints,
            user_alias.as_deref(),
        )?;
        next.alias = resolved;
        next.id = existing.id;
        next.tenant_id = existing.tenant_id;
        self.validate_upstream_bindings(tenant_id, &next).await?;
        self.upstreams.replace(next.clone()).await?;
        Ok(next)
    }

    /// Fetch an upstream owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when it does not exist.
    pub async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("upstream {id}")))
    }

    /// List upstreams owned by `tenant_id`.
    ///
    /// # Errors
    /// Never fails for the in-memory repositories.
    pub async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list_by_tenant(tenant_id).await
    }

    /// Delete an upstream and its dependent routes.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when it does not exist.
    pub async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if !self.upstreams.delete(tenant_id, id).await? {
            return Err(DomainError::NotFound(format!("upstream {id}")));
        }
        for route in self.routes.list_by_tenant(tenant_id).await? {
            if route.upstream_id == id {
                self.routes.delete(tenant_id, route.id).await?;
            }
        }
        Ok(())
    }

    /// Create a route after full validation.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails or the match rule
    /// duplicates an existing route.
    pub async fn create_route(
        &self,
        tenant_id: Uuid,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        validate_route(&route)?;
        let upstream_id = route.upstream_id;
        self.upstreams
            .get(tenant_id, upstream_id)
            .await?
            .ok_or_else(|| {
                DomainError::validation(format!(
                    "route references unknown upstream '{upstream_id}'"
                ))
            })?;
        self.validate_route_bindings(tenant_id, &route).await?;
        route.id = Uuid::new_v4();
        route.tenant_id = tenant_id;
        self.routes.insert(route.clone()).await?;
        Ok(route)
    }

    /// Replace a route. `upstream_id` is immutable.
    ///
    /// # Errors
    /// Returns [`DomainError`] when the route does not exist or validation
    /// fails.
    pub async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut next: Route,
    ) -> Result<Route, DomainError> {
        let existing = self
            .routes
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("route {id}")))?;
        next.upstream_id = existing.upstream_id;
        next.id = existing.id;
        next.tenant_id = existing.tenant_id;
        validate_route(&next)?;
        self.validate_route_bindings(tenant_id, &next).await?;
        self.routes.replace(next.clone()).await?;
        Ok(next)
    }

    /// Fetch a route owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when it does not exist.
    pub async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("route {id}")))
    }

    /// List routes owned by `tenant_id`.
    ///
    /// # Errors
    /// Never fails for the in-memory repositories.
    pub async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        self.routes.list_by_tenant(tenant_id).await
    }

    /// Delete a route.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when it does not exist.
    pub async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if !self.routes.delete(tenant_id, id).await? {
            return Err(DomainError::NotFound(format!("route {id}")));
        }
        Ok(())
    }

    /// Create an immutable plugin.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails or the name is taken.
    pub async fn create_plugin(
        &self,
        tenant_id: Uuid,
        mut plugin: Plugin,
    ) -> Result<Plugin, DomainError> {
        if plugin.name.trim().is_empty() {
            return Err(DomainError::validation("plugin name must not be empty"));
        }
        plugin.name = plugin.name.trim().to_owned();
        plugin.id = Uuid::new_v4();
        plugin.tenant_id = tenant_id;
        self.plugins.insert(plugin.clone()).await?;
        Ok(plugin)
    }

    /// Fetch a plugin owned by `tenant_id`.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when it does not exist.
    pub async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins
            .get(tenant_id, id)
            .await?
            .ok_or_else(|| DomainError::NotFound(format!("plugin {id}")))
    }

    /// List plugins owned by `tenant_id`.
    ///
    /// # Errors
    /// Never fails for the in-memory repositories.
    pub async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        self.plugins.list_by_tenant(tenant_id).await
    }

    /// Delete an unreferenced plugin.
    ///
    /// # Errors
    /// Returns [`DomainError::PluginInUse`] when an upstream or route still
    /// references the plugin, and [`DomainError::NotFound`] when it does not
    /// exist.
    pub async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let plugin = self.get_plugin(tenant_id, id).await?;
        let plugin_ref = format!("{}{}", plugin.kind.base_type(), plugin.id);

        for upstream in self.upstreams.list_by_tenant(tenant_id).await? {
            if upstream_refs_plugin(&upstream, &plugin_ref) {
                return Err(DomainError::PluginInUse(format!(
                    "plugin '{}' is still referenced by upstream '{}'",
                    plugin.name, upstream.alias
                )));
            }
        }
        for route in self.routes.list_by_tenant(tenant_id).await? {
            if route_refs_plugin(&route, &plugin_ref) {
                return Err(DomainError::PluginInUse(format!(
                    "plugin '{}' is still referenced by route {}",
                    plugin.name, route.id
                )));
            }
        }
        if !self.plugins.delete(tenant_id, id).await? {
            return Err(DomainError::NotFound(format!("plugin {id}")));
        }
        Ok(())
    }

    async fn validate_upstream_bindings(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
    ) -> Result<(), DomainError> {
        let Some(plugins) = upstream.plugins.as_ref() else {
            return Ok(());
        };
        for binding in &plugins.items {
            self.validate_plugin_binding(tenant_id, binding).await?;
        }
        Ok(())
    }

    async fn validate_route_bindings(
        &self,
        tenant_id: Uuid,
        route: &Route,
    ) -> Result<(), DomainError> {
        let Some(plugins) = route.plugins.as_ref() else {
            return Ok(());
        };
        for binding in &plugins.items {
            self.validate_plugin_binding(tenant_id, binding).await?;
        }
        Ok(())
    }

    async fn validate_plugin_binding(
        &self,
        tenant_id: Uuid,
        binding: &PluginBinding,
    ) -> Result<(), DomainError> {
        let reference = binding.plugin_ref.as_str();
        if crate::infra::plugin::is_catalog_only(reference) {
            return Err(DomainError::validation(format!(
                "plugin '{reference}' is catalog-only and has no implementation"
            )));
        }
        let stripped = reference
            .strip_prefix(gts::AUTH_PLUGIN_TYPE)
            .or_else(|| reference.strip_prefix(gts::GUARD_PLUGIN_TYPE))
            .or_else(|| reference.strip_prefix(gts::TRANSFORM_PLUGIN_TYPE));
        let Some(stripped) = stripped else {
            return Err(DomainError::validation(format!(
                "plugin reference '{reference}' is not a known OAGW plugin type"
            )));
        };
        let Ok(uuid) = Uuid::parse_str(stripped) else {
            // Built-in identifiers resolve at proxy time, not create time.
            return Ok(());
        };
        if self.plugins.get(tenant_id, uuid).await?.is_some() {
            return Ok(());
        }
        Err(DomainError::validation(format!(
            "plugin '{reference}' does not exist"
        )))
    }
}

/// Whether an upstream references `plugin_ref` anywhere in its config.
#[must_use]
pub fn upstream_refs_plugin(upstream: &Upstream, plugin_ref: &str) -> bool {
    upstream
        .plugins
        .as_ref()
        .is_some_and(|p| p.items.iter().any(|b| b.plugin_ref == plugin_ref))
}

/// Whether a route references `plugin_ref` anywhere in its config.
#[must_use]
pub fn route_refs_plugin(route: &Route, plugin_ref: &str) -> bool {
    route
        .plugins
        .as_ref()
        .is_some_and(|p| p.items.iter().any(|b| b.plugin_ref == plugin_ref))
}

/// Normalize and validate an upstream in place.
///
/// # Errors
/// Returns [`DomainError::Validation`] on any rule violation.
pub fn normalize_and_validate(
    upstream: &mut Upstream,
    allow_http_upstream: bool,
) -> Result<(), DomainError> {
    validate_protocol(&upstream.protocol)?;
    validate_endpoints(&upstream.server, allow_http_upstream)?;
    validate_tags(&upstream.tags)?;
    if let Some(cors) = upstream.cors.as_ref() {
        validate_cors(cors)?;
    }
    if let Some(rate_limit) = upstream.rate_limit.as_ref() {
        validate_rate_limit(rate_limit)?;
    }
    if let Some(headers) = upstream.headers.as_ref() {
        validate_headers(headers)?;
    }
    upstream.tags = upstream
        .tags
        .iter()
        .map(|tag| tag.to_ascii_lowercase())
        .collect();
    Ok(())
}

/// Validate a protocol identifier.
///
/// # Errors
/// Returns [`DomainError::Validation`] for unknown protocols.
pub fn validate_protocol(protocol: &str) -> Result<(), DomainError> {
    if protocol == gts::PROTOCOL_HTTP {
        return Ok(());
    }
    Err(DomainError::validation(format!(
        "unsupported protocol '{protocol}'"
    )))
}

/// Validate the endpoint pool of an upstream.
///
/// # Errors
/// Returns [`DomainError::Validation`] on any rule violation.
pub fn validate_endpoints(
    server: &crate::domain::model::ServerConfig,
    allow_http_upstream: bool,
) -> Result<(), DomainError> {
    if server.endpoints.is_empty() {
        return Err(DomainError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    }
    let first = &server.endpoints[0];
    for endpoint in &server.endpoints {
        validate_endpoint(endpoint, allow_http_upstream)?;
        if endpoint.scheme != first.scheme {
            return Err(DomainError::validation(
                "all pooled endpoints must share the same scheme",
            ));
        }
        if endpoint.port != first.port {
            return Err(DomainError::validation(
                "all pooled endpoints must share the same port",
            ));
        }
    }
    Ok(())
}

/// Validate a single endpoint.
///
/// # Errors
/// Returns [`DomainError::Validation`] on any rule violation.
pub fn validate_endpoint(
    endpoint: &Endpoint,
    allow_http_upstream: bool,
) -> Result<(), DomainError> {
    if !alias::is_valid_host(&endpoint.host) {
        return Err(DomainError::validation(format!(
            "invalid endpoint host '{}'",
            endpoint.host
        )));
    }
    if endpoint.port == 0 {
        return Err(DomainError::validation("endpoint port must be 1..=65535"));
    }
    if !allow_http_upstream && !endpoint.scheme.is_tls() {
        return Err(DomainError::validation(
            "plaintext upstream endpoints require allow_http_upstream",
        ));
    }
    Ok(())
}

/// Validate discovery tags.
///
/// # Errors
/// Returns [`DomainError::Validation`] when a tag is malformed.
pub fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.len() <= 64
            && tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !valid {
            return Err(DomainError::validation(format!(
                "invalid tag '{tag}': must match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(())
}

/// Validate CORS configuration.
///
/// # Errors
/// Returns [`DomainError::Validation`] when credentials are combined with a
/// wildcard origin.
pub fn validate_cors(cors: &crate::domain::model::CorsConfig) -> Result<(), DomainError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::validation(
            "cors.allow_credentials cannot be combined with a wildcard origin",
        ));
    }
    Ok(())
}

/// Validate a rate-limit configuration.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the sustained rate is missing or
/// zero, or the burst capacity is zero.
pub fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<(), DomainError> {
    let Some(sustained) = rate_limit.sustained else {
        return Err(DomainError::validation(
            "rate_limit.sustained is required when a rate limit is configured",
        ));
    };
    if sustained.rate == 0 {
        return Err(DomainError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if rate_limit.burst.is_some_and(|b| b.capacity == 0) {
        return Err(DomainError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if rate_limit.cost == 0 {
        return Err(DomainError::validation(
            "rate_limit.cost must be at least 1",
        ));
    }
    Ok(())
}

/// Validate header transformation rules.
///
/// # Errors
/// Returns [`DomainError::Validation`] when a rule is malformed.
pub fn validate_headers(headers: &HeadersConfig) -> Result<(), DomainError> {
    for name in headers
        .request
        .remove
        .iter()
        .chain(headers.response.remove.iter())
    {
        if name.is_empty() {
            return Err(DomainError::validation(
                "header remove names must not be empty",
            ));
        }
    }
    Ok(())
}

/// Validate a route.
///
/// # Errors
/// Returns [`DomainError::Validation`] on any rule violation.
pub fn validate_route(route: &Route) -> Result<(), DomainError> {
    validate_match(&route.match_rule)?;
    let http = route
        .match_rule
        .http
        .as_ref()
        .ok_or_else(|| DomainError::validation("route.match.http is required"))?;

    if http.methods.is_empty() {
        return Err(DomainError::validation(
            "route.match.http.methods must contain at least one method",
        ));
    }
    for method in &http.methods {
        if HttpMethod::parse(method).is_none() {
            return Err(DomainError::validation(format!(
                "unsupported HTTP method '{method}'"
            )));
        }
    }
    if http.path.is_empty() || !http.path.starts_with('/') {
        return Err(DomainError::validation(
            "route.match.http.path must start with '/'",
        ));
    }
    if let Some(cors) = route.cors.as_ref() {
        validate_cors(cors)?;
    }
    if let Some(rate_limit) = route.rate_limit.as_ref() {
        validate_rate_limit(rate_limit)?;
    }
    validate_tags(&route.tags)?;
    Ok(())
}

/// Validate a `MatchConfig` shape.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the config mixes protocols.
pub fn validate_match(match_rule: &MatchConfig) -> Result<(), DomainError> {
    if match_rule.http.is_some() && match_rule.grpc.is_some() {
        return Err(DomainError::validation(
            "route.match must not declare both http and grpc",
        ));
    }
    Ok(())
}
