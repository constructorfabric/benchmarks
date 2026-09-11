//! Request and response DTOs of the management API.
//!
//! The request shapes mirror `docs/schemas/*.v1.schema.json` exactly — including
//! `deny_unknown_fields`, so a payload the schema would reject is rejected here too.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::gts_helpers;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, RateLimitConfig, RouteMatch, ServerConfig,
};

/// Request body of `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    /// Caller-supplied alias; must match the derived alias when one can be derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Categorisation tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Endpoints the upstream is reachable at.
    pub server: ServerConfig,
    /// Protocol of the upstream service.
    pub protocol: String,
    /// Outbound credential injection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// Request body of `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    /// Upstream the route belongs to. Immutable after creation.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub route_match: RouteMatch,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Categorisation tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
}

/// Request body of `POST /oagw/v1/plugins`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    /// Human-readable name.
    pub name: String,
    /// Free-form description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Plugin base type (`auth`/`guard`/`transform`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Phases the plugin participates in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phases: Option<Vec<String>>,
    /// JSON schema of the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Plugin source (Starlark), not executed in this release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
}

/// Request body of the enable/disable endpoints.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnabledRequest {
    /// Desired state.
    pub enabled: bool,
}

/// Stored upstream as returned by the management API.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamResponse {
    /// System-generated id.
    pub id: Uuid,
    /// GTS id of the stored resource.
    pub gts_id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing alias, after derivation.
    pub alias: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Protocol identifier.
    pub protocol: String,
    /// Endpoints.
    pub server: ServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<crate::domain::model::Upstream> for UpstreamResponse {
    fn from(upstream: crate::domain::model::Upstream) -> Self {
        Self {
            id: upstream.id,
            gts_id: gts_helpers::upstream_gts(upstream.id),
            tenant_id: upstream.tenant_id,
            alias: upstream.alias,
            enabled: upstream.enabled,
            protocol: upstream.protocol,
            server: upstream.server,
            auth: upstream.auth,
            headers: upstream.headers,
            plugins: upstream.plugins,
            rate_limit: upstream.rate_limit,
            cors: upstream.cors,
            tags: upstream.tags,
        }
    }
}

/// Stored route as returned by the management API.
#[derive(Debug, Clone, Serialize)]
pub struct RouteResponse {
    /// System-generated id.
    pub id: Uuid,
    /// GTS id of the stored resource.
    pub gts_id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Upstream the route belongs to.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub route_match: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<crate::domain::model::Route> for RouteResponse {
    fn from(route: crate::domain::model::Route) -> Self {
        Self {
            id: route.id,
            gts_id: gts_helpers::route_gts(route.id),
            tenant_id: route.tenant_id,
            upstream_id: route.upstream_id,
            enabled: route.enabled,
            route_match: route.route_match,
            plugins: route.plugins,
            rate_limit: route.rate_limit,
            cors: route.cors,
            tags: route.tags,
        }
    }
}

/// Stored custom plugin as returned by the management API.
#[derive(Debug, Clone, Serialize)]
pub struct PluginResponse {
    /// System-generated id.
    pub id: Uuid,
    /// GTS id of the stored resource.
    pub gts_id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Human-readable name.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// True while an upstream or route still references the plugin.
    pub in_use: bool,
    /// Resources referencing the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub referenced_by: Vec<String>,
}

/// A plugin in the built-in catalogue.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogEntry {
    /// GTS identifier of the plugin.
    pub id: &'static str,
    /// `auth`, `guard` or `transform`.
    pub kind: &'static str,
    /// True when a built-in implementation is registered and bindable.
    pub resolvable: bool,
}

/// Response of `GET /oagw/v1/plugins` — the catalogue plus the tenant's own definitions.
#[derive(Debug, Clone, Serialize)]
pub struct PluginCatalogResponse {
    /// Built-in plugin identifiers, including the catalog-only ones.
    pub builtins: Vec<CatalogEntry>,
    /// The tenant's own plugin definitions.
    pub plugins: Vec<PluginResponse>,
}

impl PluginResponse {
    /// Response for a stored plugin, annotated with the resources that reference it.
    #[must_use]
    pub fn from_plugin(plugin: crate::domain::model::Plugin, referenced_by: Vec<String>) -> Self {
        let gts_id = gts_helpers::plugin_gts(plugin.id);
        let in_use = !referenced_by.is_empty();
        Self {
            id: plugin.id,
            gts_id,
            tenant_id: plugin.tenant_id,
            name: plugin.name,
            description: plugin.description,
            plugin_type: plugin.plugin_type,
            phases: plugin.phases,
            config_schema: plugin.config_schema,
            source_code: plugin.source_code,
            in_use,
            referenced_by,
        }
    }
}
