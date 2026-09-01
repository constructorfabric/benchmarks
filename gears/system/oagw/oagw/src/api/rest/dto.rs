//! REST transport DTOs.
//!
//! The read side reuses the domain model types directly (they already carry
//! the wire schema shapes); the write side uses these request bodies, which
//! omit the server-owned `id`, `tenant_id` and `created_at` fields.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, Plugin, PluginType, PluginsConfig,
    RateLimitConfig, Route, ServerConfig, Upstream,
};
use crate::domain::services::management::ListQuery;

/// Nil UUID used as a placeholder for server-generated identifiers.
const NIL: Uuid = Uuid::nil();

/// Pagination / filtering query parameters (`$top`, `$skip`, `$filter`,
/// `$orderby`, `$select`), with plain-name aliases accepted as well.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListParamsDto {
    /// Maximum number of items.
    #[serde(default, rename = "$top", alias = "top")]
    pub top: Option<u64>,
    /// Number of items to skip.
    #[serde(default, rename = "$skip", alias = "skip")]
    pub skip: Option<u64>,
    /// OData filter expression.
    #[serde(default, rename = "$filter", alias = "filter")]
    pub filter: Option<String>,
    /// Sort expression.
    #[serde(default, rename = "$orderby", alias = "orderby")]
    pub orderby: Option<String>,
    /// Projected fields, comma separated.
    #[serde(default, rename = "$select", alias = "select")]
    pub select: Option<String>,
}

impl ListParamsDto {
    /// Converts to the domain [`ListQuery`].
    #[must_use]
    pub fn to_list_query(&self) -> ListQuery {
        ListQuery {
            top: self.top,
            skip: self.skip,
            filter: self.filter.clone(),
            orderby: self.orderby.clone(),
            select: self.select.as_ref().and_then(|value| {
                let names: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
                (!names.is_empty()).then_some(names)
            }),
        }
    }
}

/// Query parameters of `GET /plugins`.
///
/// Combines the OData parameters documented in `DESIGN.md` §3.3 with the
/// `type` / `plugin_type` shorthand (`?type=guard`), which is applied in
/// addition to whatever `$filter` expression is forwarded.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginListParams {
    /// Filter by plugin type.
    #[serde(default, rename = "type", alias = "plugin_type")]
    pub plugin_type: Option<PluginTypeDto>,
    /// OData filter expression.
    #[serde(default, rename = "$filter", alias = "filter")]
    pub filter: Option<String>,
    /// Sort expression.
    #[serde(default, rename = "$orderby", alias = "orderby")]
    pub orderby: Option<String>,
    /// Projected fields, comma separated.
    #[serde(default, rename = "$select", alias = "select")]
    pub select: Option<String>,
    /// Maximum number of items.
    #[serde(default, rename = "$top", alias = "top")]
    pub top: Option<u64>,
    /// Number of items to skip.
    #[serde(default, rename = "$skip", alias = "skip")]
    pub skip: Option<u64>,
}

impl PluginListParams {
    /// Converts the OData fields to the domain [`ListQuery`].
    #[must_use]
    pub fn to_list_query(&self) -> ListQuery {
        ListParamsDto {
            top: self.top,
            skip: self.skip,
            filter: self.filter.clone(),
            orderby: self.orderby.clone(),
            select: self.select.clone(),
        }
        .to_list_query()
    }
}

/// REST shape of [`PluginType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginTypeDto {
    /// Credential injection.
    Auth,
    /// Validation / policy.
    Guard,
    /// Request / response mutation.
    Transform,
}

impl From<PluginTypeDto> for PluginType {
    fn from(value: PluginTypeDto) -> Self {
        match value {
            PluginTypeDto::Auth => PluginType::Auth,
            PluginTypeDto::Guard => PluginType::Guard,
            PluginTypeDto::Transform => PluginType::Transform,
        }
    }
}

impl From<PluginType> for PluginTypeDto {
    fn from(value: PluginType) -> Self {
        match value {
            PluginType::Auth => PluginTypeDto::Auth,
            PluginType::Guard => PluginTypeDto::Guard,
            PluginType::Transform => PluginTypeDto::Transform,
        }
    }
}

/// Serde default for the `enabled` flag (true).
fn default_enabled() -> bool {
    true
}

/// Request body of the upstream create/replace endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamBodyDto {
    /// Routing key used in `/proxy/{alias}/...`; derived from the endpoints
    /// when omitted for a hostname-based upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Upstream protocol (GTS identifier).
    #[serde(default)]
    pub protocol: String,
    /// Disabled upstreams reject every request.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Server endpoint pool.
    pub server: ServerConfig,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl UpstreamBodyDto {
    /// Converts to the domain model with placeholder identity fields.
    #[must_use]
    pub fn into_upstream(self) -> Upstream {
        Upstream {
            id: NIL,
            tenant_id: NIL,
            alias: self.alias.unwrap_or_default(),
            protocol: self.protocol,
            enabled: self.enabled,
            server: self.server,
            auth: self.auth,
            headers: self.headers,
            rate_limit: self.rate_limit,
            cors: self.cors,
            plugins: self.plugins,
            tags: self.tags,
            created_at: 0,
        }
    }

    /// Builds the DTO from a stored upstream, dropping the server-owned
    /// identity fields so the result round-trips through `PUT`.
    #[must_use]
    pub fn from_upstream(upstream: &Upstream) -> Self {
        Self {
            alias: Some(upstream.alias.clone()),
            protocol: upstream.protocol.clone(),
            enabled: upstream.enabled,
            server: upstream.server.clone(),
            auth: upstream.auth.clone(),
            headers: upstream.headers.clone(),
            rate_limit: upstream.rate_limit.clone(),
            cors: upstream.cors.clone(),
            plugins: upstream.plugins.clone(),
            tags: upstream.tags.clone(),
        }
    }
}

/// Request body of the route create endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteCreateDto {
    /// Upstream the route belongs to.
    pub upstream_id: Uuid,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Sort priority for equal-prefix matches.
    #[serde(default)]
    pub priority: u32,
    /// Disabled routes are skipped during resolution.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Request body of the route replace endpoint (`upstream_id` is immutable and
/// therefore absent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteUpdateDto {
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Sort priority for equal-prefix matches.
    #[serde(default)]
    pub priority: u32,
    /// Disabled routes are skipped during resolution.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl RouteCreateDto {
    /// Converts to the domain model with placeholder identity fields.
    #[must_use]
    pub fn into_route(self) -> Route {
        Route {
            id: NIL,
            tenant_id: NIL,
            upstream_id: self.upstream_id,
            match_config: self.match_config,
            priority: self.priority,
            enabled: self.enabled,
            rate_limit: self.rate_limit,
            cors: self.cors,
            plugins: self.plugins,
            tags: self.tags,
            created_at: 0,
        }
    }
}

impl RouteUpdateDto {
    /// Converts to the domain model; the upstream binding is filled in by the
    /// service from the stored route.
    #[must_use]
    pub fn into_route(self) -> Route {
        Route {
            id: NIL,
            tenant_id: NIL,
            upstream_id: NIL,
            match_config: self.match_config,
            priority: self.priority,
            enabled: self.enabled,
            rate_limit: self.rate_limit,
            cors: self.cors,
            plugins: self.plugins,
            tags: self.tags,
            created_at: 0,
        }
    }

    /// Builds the DTO from a stored route, dropping the immutable binding.
    #[must_use]
    pub fn from_route(route: &Route) -> Self {
        Self {
            match_config: route.match_config.clone(),
            priority: route.priority,
            enabled: route.enabled,
            rate_limit: route.rate_limit.clone(),
            cors: route.cors.clone(),
            plugins: route.plugins.clone(),
            tags: route.tags.clone(),
        }
    }
}

/// Request body of the plugin create endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCreateDto {
    /// Plugin type (`auth` | `guard` | `transform`).
    #[serde(rename = "type")]
    pub plugin_type: PluginTypeDto,
    /// Unique name within the tenant.
    pub name: String,
    /// JSON Schema describing the accepted plugin configuration.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Starlark source code.
    #[serde(default)]
    pub source_code: String,
}

impl PluginCreateDto {
    /// Converts to the domain model with placeholder identity fields.
    #[must_use]
    pub fn into_plugin(self) -> Plugin {
        Plugin {
            id: NIL,
            tenant_id: NIL,
            plugin_type: self.plugin_type.into(),
            name: self.name,
            config_schema: self.config_schema,
            source_code: self.source_code,
            last_used_at: None,
            gc_eligible_at: None,
            created_at: 0,
        }
    }
}

/// `GET /plugins/{id}/source` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSourceDto {
    /// GTS instance id of the plugin.
    pub plugin_id: String,
    /// Plugin type.
    #[serde(rename = "type")]
    pub plugin_type: PluginTypeDto,
    /// Starlark source code.
    pub source_code: String,
}
