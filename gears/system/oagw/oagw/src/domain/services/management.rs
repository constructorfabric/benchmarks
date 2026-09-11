//! Control-plane service: upstream, route and plugin lifecycle.
//!
//! This is the domain's management surface — validation, alias derivation,
//! match-rule uniqueness and OData-ish listing — independent of HTTP.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::domain::dto::{
    Endpoint, GrpcMatch, MatchRule, Plugin, Route, ServerConfig, Upstream,
};
use crate::domain::error::DomainError;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

/// Tag pattern every `tags` entry must match.
const TAG_PATTERN: &str = "^[a-z0-9_-]+$";

/// Writes that must clear the data plane's L1 cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidation {
    /// An upstream changed.
    Upstream,
    /// A route changed.
    Route,
}

/// Control-plane operations.
#[async_trait]
pub trait ControlPlane: Send + Sync {
    /// Creates an upstream.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails or the alias is taken.
    async fn create_upstream(
        &self,
        tenant_id: Uuid,
        upstream: Upstream,
    ) -> Result<Upstream, DomainError>;

    /// Replaces an upstream.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails or the resource is absent.
    async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        upstream: Upstream,
    ) -> Result<Upstream, DomainError>;

    /// Deletes an upstream and every route that references it.
    ///
    /// # Errors
    /// Returns [`DomainError`] when the resource is absent.
    async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Reads an upstream.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;

    /// Fetches an upstream by alias.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get_upstream_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Upstream, DomainError>;

    /// Lists upstreams.
    ///
    /// # Errors
    /// Returns [`DomainError`] when the store fails.
    async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Creates a route.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails.
    async fn create_route(&self, tenant_id: Uuid, route: Route) -> Result<Route, DomainError>;

    /// Replaces a route; `upstream_id` is immutable.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails.
    async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        route: Route,
    ) -> Result<Route, DomainError>;

    /// Deletes a route.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when absent.
    async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Reads a route.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;

    /// Lists routes.
    ///
    /// # Errors
    /// Returns [`DomainError`] when the store fails.
    async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Creates a plugin.
    ///
    /// # Errors
    /// Returns [`DomainError`] when validation fails.
    async fn create_plugin(&self, tenant_id: Uuid, plugin: Plugin) -> Result<Plugin, DomainError>;

    /// Deletes a plugin, refusing while it is still referenced.
    ///
    /// # Errors
    /// Returns [`DomainError::PluginInUse`] when referenced.
    async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    /// Reads a plugin.
    ///
    /// # Errors
    /// Returns [`DomainError::NotFound`] when absent.
    async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;

    /// Lists plugins.
    ///
    /// # Errors
    /// Returns [`DomainError`] when the store fails.
    async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;
}

/// The in-process control plane.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    plugins: Arc<dyn PluginRepository>,
}

impl ControlPlaneService {
    /// Builds a control plane over the given repositories.
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

    /// The repositories behind this service, for the data plane's resolution
    /// walk.
    ///
    // The tuple is spelled out because it is this crate's public API shape;
    // introducing a type alias would add a new public name.
    #[allow(clippy::type_complexity)]
    #[must_use]
    pub fn repositories(
        &self,
    ) -> (
        Arc<dyn UpstreamRepository>,
        Arc<dyn RouteRepository>,
        Arc<dyn PluginRepository>,
    ) {
        (
            Arc::clone(&self.upstreams),
            Arc::clone(&self.routes),
            Arc::clone(&self.plugins),
        )
    }

    /// Validates an upstream document.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when a rule is violated.
    ///
    // Kept as a method so callers keep using `service.validate_upstream(..)`;
    // it reads no state from `self`.
    #[allow(clippy::unused_self)]
    pub fn validate_upstream(&self, upstream: &Upstream) -> Result<(), DomainError> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(DomainError::Validation(
                "server.endpoints must contain at least one endpoint".into(),
            ));
        }
        for endpoint in endpoints {
            validate_endpoint(endpoint)?;
        }
        if endpoints.len() > 1 {
            let first = &endpoints[0];
            if endpoints
                .iter()
                .any(|endpoint| endpoint.scheme != first.scheme || endpoint.port != first.port)
            {
                return Err(DomainError::Validation(
                    "all endpoints in one pool must share scheme, protocol and port".into(),
                ));
            }
        }
        for tag in &upstream.tags {
            if !is_valid_tag(tag) {
                return Err(DomainError::Validation(format!(
                    "tag `{tag}` must match {TAG_PATTERN}"
                )));
            }
        }
        if let Some(cors) = &upstream.cors {
            validate_cors(cors)?;
        }
        if let Some(rate_limit) = &upstream.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(auth) = &upstream.auth {
            validate_auth_plugin_reference(&auth.auth_type)?;
        }
        if let Some(plugins) = &upstream.plugins {
            for reference in &plugins.items {
                validate_bindable_plugin_reference(reference)?;
            }
        }
        Ok(())
    }

    /// Validates a route document.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when a rule is violated.
    ///
    // Kept as a method so callers keep using `service.validate_route(..)`;
    // it reads no state from `self`.
    #[allow(clippy::unused_self)]
    pub fn validate_route(&self, route: &Route) -> Result<(), DomainError> {
        match &route.match_rule {
            crate::domain::dto::MatchRule::Http(http) => {
                if http.methods.is_empty() {
                    return Err(DomainError::Validation(
                        "match.http.methods must not be empty".into(),
                    ));
                }
                if http.path.is_empty() {
                    return Err(DomainError::Validation("match.http.path is required".into()));
                }
                if !http.path.starts_with('/') {
                    return Err(DomainError::Validation(
                        "match.http.path must be an absolute path".into(),
                    ));
                }
            }
            crate::domain::dto::MatchRule::Grpc(GrpcMatch { service, method }) => {
                if service.is_empty() || method.is_empty() {
                    return Err(DomainError::Validation(
                        "match.grpc requires both service and method".into(),
                    ));
                }
            }
        }
        for tag in &route.tags {
            if !is_valid_tag(tag) {
                return Err(DomainError::Validation(format!(
                    "tag `{tag}` must match {TAG_PATTERN}"
                )));
            }
        }
        if let Some(cors) = &route.cors {
            validate_cors(cors)?;
        }
        if let Some(rate_limit) = &route.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(plugins) = &route.plugins {
            for reference in &plugins.items {
                validate_bindable_plugin_reference(reference)?;
            }
        }
        Ok(())
    }

    /// Resolves every UUID-backed entry in a plugin chain against the plugin
    /// repository.
    ///
    /// A named identifier is checked by [`validate_bindable_plugin_reference`]
    /// alone; a UUID instance names a custom plugin stored in `oagw_plugin`, so
    /// the reference only binds if a row is actually there.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when a UUID instance names no plugin
    /// this tenant owns.
    async fn ensure_plugins_resolvable(
        &self,
        tenant_id: Uuid,
        plugins: Option<&crate::domain::dto::PluginsConfig>,
    ) -> Result<(), DomainError> {
        let Some(plugins) = plugins else {
            return Ok(());
        };
        for reference in &plugins.items {
            let Some(id) = crate::domain::gts_helpers::plugin_uuid_of(reference) else {
                continue;
            };
            if self
                .plugins
                .find_by_id(tenant_id, id)
                .await
                .map_err(DomainError::from)?
                .is_none()
            {
                return Err(DomainError::Validation(format!(
                    "plugin reference `{reference}` does not name a plugin in this tenant"
                )));
            }
        }
        Ok(())
    }

    /// Rejects a match rule that collides with an existing one under the same
    /// upstream.
    ///
    /// # Errors
    /// Returns [`DomainError::Conflict`] on a duplicate.
    pub async fn ensure_match_unique(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        candidate: &MatchRule,
        ignore_route_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        let existing = self
            .routes
            .list_by_upstream(upstream_id)
            .await
            .map_err(DomainError::from)?;
        for route in existing {
            if route.tenant_id != tenant_id || Some(route.id) == ignore_route_id {
                continue;
            }
            if match_rules_conflict(&route.match_rule, candidate) {
                return Err(DomainError::Conflict(format!(
                    "a route with the same path, priority and methods already exists ({})",
                    route.id
                )));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ControlPlane for ControlPlaneService {
    async fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        self.validate_upstream(&upstream)?;
        self.ensure_plugins_resolvable(tenant_id, upstream.plugins.as_ref()).await?;
        upstream.tenant_id = tenant_id;
        let provided = if upstream.alias.trim().is_empty() {
            None
        } else {
            Some(upstream.alias.as_str())
        };
        let alias = crate::domain::alias::enforce_alias_create(&upstream.server.endpoints, provided)?;
        upstream.alias = alias;
        if self
            .upstreams
            .find_by_alias(tenant_id, &upstream.alias)
            .await
            .map_err(DomainError::from)?
            .is_some()
        {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias `{}` already exists in this tenant",
                upstream.alias
            )));
        }
        let outcome = self
            .upstreams
            .insert(upstream.clone())
            .await
            .map_err(DomainError::from)?;
        if outcome == crate::domain::repo::WriteOutcome::KeyExists {
            return Err(DomainError::Conflict(format!(
                "an upstream with alias `{}` already exists in this tenant",
                upstream.alias
            )));
        }
        Ok(upstream)
    }

    async fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut upstream: Upstream,
    ) -> Result<Upstream, DomainError> {
        self.validate_upstream(&upstream)?;
        self.ensure_plugins_resolvable(tenant_id, upstream.plugins.as_ref()).await?;
        let existing = self
            .upstreams
            .find_by_id(tenant_id, id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound("upstream not found".into()))?;
        let provided = if upstream.alias.trim().is_empty() {
            None
        } else {
            Some(upstream.alias.as_str())
        };
        let alias = crate::domain::alias::enforce_alias_update(
            &existing.server.endpoints,
            &existing.alias,
            &upstream.server.endpoints,
            provided,
        )?;
        upstream.id = existing.id;
        upstream.tenant_id = existing.tenant_id;
        upstream.alias = alias;
        upstream.created_at = existing.created_at.clone();
        self.upstreams
            .update(upstream.clone())
            .await
            .map_err(DomainError::from)?;
        Ok(upstream)
    }

    async fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let deleted = self
            .upstreams
            .delete(tenant_id, id)
            .await
            .map_err(DomainError::from)?;
        if !deleted {
            return Err(DomainError::NotFound("upstream not found".into()));
        }
        for route in self.routes.list_by_upstream(id).await.map_err(DomainError::from)? {
            if route.tenant_id == tenant_id {
                // Best-effort cascade: a stray route that cannot be removed
                // must not fail the upstream deletion, so its result is
                // dropped here.
                drop(self.routes.delete(tenant_id, route.id).await);
            }
        }
        Ok(())
    }

    async fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .find_by_id(tenant_id, id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound("upstream not found".into()))
    }

    async fn get_upstream_by_alias(
        &self,
        tenant_id: Uuid,
        alias: &str,
    ) -> Result<Upstream, DomainError> {
        self.upstreams
            .find_by_alias(tenant_id, alias)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound(format!("no upstream for alias `{alias}`")))
    }

    async fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list(tenant_id).await.map_err(DomainError::from)
    }

    async fn create_route(&self, tenant_id: Uuid, mut route: Route) -> Result<Route, DomainError> {
        self.validate_route(&route)?;
        self.ensure_plugins_resolvable(tenant_id, route.plugins.as_ref()).await?;
        let upstream = self
            .upstreams
            .find_by_id(tenant_id, route.upstream_id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| {
                DomainError::Validation("upstream_id does not name an upstream in this tenant".into())
            })?;
        self.ensure_match_unique(tenant_id, upstream.id, &route.match_rule, None)
            .await?;
        route.tenant_id = tenant_id;
        route.id = Uuid::new_v4();
        self.routes.insert(route.clone()).await.map_err(DomainError::from)?;
        Ok(route)
    }

    async fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut route: Route,
    ) -> Result<Route, DomainError> {
        self.validate_route(&route)?;
        self.ensure_plugins_resolvable(tenant_id, route.plugins.as_ref()).await?;
        let existing = self
            .routes
            .find_by_id(tenant_id, id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound("route not found".into()))?;
        self.ensure_match_unique(tenant_id, existing.upstream_id, &route.match_rule, Some(existing.id))
            .await?;
        route.id = existing.id;
        route.tenant_id = existing.tenant_id;
        route.upstream_id = existing.upstream_id;
        route.created_at = existing.created_at.clone();
        self.routes.update(route.clone()).await.map_err(DomainError::from)?;
        Ok(route)
    }

    async fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let deleted = self
            .routes
            .delete(tenant_id, id)
            .await
            .map_err(DomainError::from)?;
        if !deleted {
            return Err(DomainError::NotFound("route not found".into()));
        }
        Ok(())
    }

    async fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes
            .find_by_id(tenant_id, id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound("route not found".into()))
    }

    async fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        self.routes.list(tenant_id).await.map_err(DomainError::from)
    }

    async fn create_plugin(&self, tenant_id: Uuid, mut plugin: Plugin) -> Result<Plugin, DomainError> {
        if plugin.name.trim().is_empty() {
            return Err(DomainError::Validation("plugin name is required".into()));
        }
        plugin.tenant_id = tenant_id;
        plugin.id = Uuid::new_v4();
        self.plugins.insert(plugin.clone()).await.map_err(DomainError::from)?;
        Ok(plugin)
    }

    async fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.plugins
            .find_by_id(tenant_id, id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound("plugin not found".into()))?;
        let referenced = |items: &[String]| {
            items
                .iter()
                // A custom plugin is bound by its GTS identifier, whose
                // instance part is the id minted here; a bare id also counts.
                .any(|item| crate::domain::gts_helpers::plugin_uuid_of(item) == Some(id))
        };
        for upstream in self.upstreams.list(tenant_id).await.map_err(DomainError::from)? {
            if upstream
                .plugins
                .as_ref()
                .is_some_and(|plugins| referenced(&plugins.items))
            {
                return Err(DomainError::PluginInUse(
                    "plugin is referenced by an upstream".into(),
                ));
            }
        }
        for route in self.routes.list(tenant_id).await.map_err(DomainError::from)? {
            if route
                .plugins
                .as_ref()
                .is_some_and(|plugins| referenced(&plugins.items))
            {
                return Err(DomainError::PluginInUse(
                    "plugin is referenced by a route".into(),
                ));
            }
        }
        self.plugins.delete(tenant_id, id).await.map_err(DomainError::from)?;
        Ok(())
    }

    async fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins
            .find_by_id(tenant_id, id)
            .await
            .map_err(DomainError::from)?
            .ok_or_else(|| DomainError::NotFound("plugin not found".into()))
    }

    async fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        self.plugins.list(tenant_id).await.map_err(DomainError::from)
    }
}

/// Validates the `auth.type` an upstream declares.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the identifier is not a resolvable
/// auth plugin, including the catalog-only `basic` and `bearer` identifiers.
pub fn validate_auth_plugin_reference(identifier: &str) -> Result<(), DomainError> {
    if crate::domain::gts_helpers::is_catalog_only_plugin(identifier) {
        return Err(DomainError::Validation(format!(
            "auth plugin `{identifier}` is cataloged but has no runtime implementation"
        )));
    }
    let known = [
        crate::domain::gts_helpers::AUTH_PLUGIN_NOOP,
        crate::domain::gts_helpers::AUTH_PLUGIN_APIKEY,
        crate::domain::gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED,
        crate::domain::gts_helpers::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC,
    ];
    if known.contains(&identifier) {
        return Ok(());
    }
    // A custom plugin is referenced by the UUID the CP minted for it, either
    // bare or as the instance part of a full GTS identifier.
    if crate::domain::gts_helpers::plugin_uuid_of(identifier).is_some() {
        return Ok(());
    }
    Err(DomainError::Validation(format!(
        "`{identifier}` is not a known auth plugin identifier"
    )))
}

/// Validates a `plugins.items[]` reference.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the reference names a catalog-only
/// plugin or is not a resolvable identifier.
pub fn validate_bindable_plugin_reference(reference: &str) -> Result<(), DomainError> {
    if crate::domain::gts_helpers::is_catalog_only_plugin(reference) {
        return Err(DomainError::Validation(format!(
            "plugin `{reference}` is core data-plane logic and cannot be bound through plugins.items"
        )));
    }
    if reference == crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID
        || reference == crate::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID
    {
        return Ok(());
    }
    // A custom plugin's instance part is the UUID the CP minted for it.
    if crate::domain::gts_helpers::plugin_uuid_of(reference).is_some() {
        return Ok(());
    }
    Err(DomainError::Validation(format!(
        "plugin reference `{reference}` does not name a bindable plugin"
    )))
}

/// Whether a tag matches `^[a-z0-9_-]+$`.
#[must_use]
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
}

fn validate_endpoint(endpoint: &Endpoint) -> Result<(), DomainError> {
    if crate::domain::alias::is_ip_literal(&endpoint.host) {
        return Ok(());
    }
    if !crate::domain::alias::is_valid_hostname(&endpoint.host) {
        return Err(DomainError::Validation(format!(
            "host `{}` is not a valid RFC 1123 hostname or IP literal",
            endpoint.host
        )));
    }
    Ok(())
}

/// Validates the CORS block: credentials and a wildcard origin are mutually
/// exclusive.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the block is contradictory.
pub fn validate_cors(cors: &crate::domain::dto::CorsConfig) -> Result<(), DomainError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(DomainError::Validation(
            "cors.allow_credentials cannot be combined with a wildcard origin".into(),
        ));
    }
    Ok(())
}

/// Validates the rate-limit block: `sustained.rate` is required.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the block is incomplete.
pub fn validate_rate_limit(rate_limit: &crate::domain::dto::RateLimitConfig) -> Result<(), DomainError> {
    if rate_limit.sustained.rate == 0 {
        return Err(DomainError::Validation(
            "rate_limit.sustained.rate is required and must be at least 1".into(),
        ));
    }
    if rate_limit.capacity() == 0 {
        return Err(DomainError::Validation("rate_limit.burst.capacity must be at least 1".into()));
    }
    Ok(())
}

/// Whether two match rules claim the same path, priority and methods.
#[must_use]
pub fn match_rules_conflict(left: &MatchRule, right: &MatchRule) -> bool {
    match (left, right) {
        (MatchRule::Http(left), MatchRule::Http(right)) => {
            let same_path = left.path == right.path;
            let same_priority = left.path.len() == right.path.len();
            let overlapping = left.methods.iter().any(|method| right.methods.contains(method));
            same_path && same_priority && overlapping
        }
        (MatchRule::Grpc(left), MatchRule::Grpc(right)) => {
            left.service == right.service && left.method == right.method
        }
        _ => false,
    }
}

/// Validates a `ServerConfig`, shared by the wire DTO layer.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the pool is empty or heterogeneous.
pub fn validate_server(server: &ServerConfig) -> Result<(), DomainError> {
    if server.endpoints.is_empty() {
        return Err(DomainError::Validation(
            "server.endpoints must contain at least one endpoint".into(),
        ));
    }
    let first = &server.endpoints[0];
    for endpoint in &server.endpoints {
        if endpoint.scheme != first.scheme || endpoint.port != first.port {
            return Err(DomainError::Validation(
                "all endpoints in one pool must share scheme, protocol and port".into(),
            ));
        }
        validate_endpoint(endpoint)?;
    }
    Ok(())
}
