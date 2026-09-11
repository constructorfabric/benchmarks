//! Request and response shapes for the management API.

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginType, PluginsConfig, RateLimitConfig,
    ServerConfig, TransformPhase,
};
use uuid::Uuid;

/// Create or replace an upstream.
#[derive(Clone)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct UpstreamWriteDto {
    /// Server-generated identifier, accepted and ignored so a document read
    /// back from the API can be written straight back. It is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub id: Option<String>,
    /// Raw identifier, accepted and ignored for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub uuid: Option<String>,
    /// Routing key; derived from hostname endpoints when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Endpoint pool.
    #[schema(value_type = Object)]
    pub server: ServerConfig,
    /// Protocol classification.
    pub protocol: String,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Outbound credential configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub headers: Option<HeadersConfig>,
    /// Plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cors: Option<CorsConfig>,
}

const fn default_true() -> bool {
    true
}

/// An upstream as returned by the management API.
#[derive(Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Anonymous global type system identifier.
    pub id: String,
    /// Raw identifier.
    #[schema(value_type = String)]
    pub uuid: Uuid,
    /// Routing key.
    pub alias: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Endpoint pool.
    #[schema(value_type = Object)]
    pub server: ServerConfig,
    /// Protocol classification.
    pub protocol: String,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Outbound credential configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub headers: Option<HeadersConfig>,
    /// Plugin bindings.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cors: Option<CorsConfig>,
}

/// A page of upstreams.
#[derive(Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamListDto {
    /// The page's items.
    pub items: Vec<UpstreamDto>,
    /// Number of items in this page.
    pub count: usize,
}

/// Create a route.
#[derive(Clone)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RouteCreateDto {
    /// Server-generated identifier, accepted and ignored so a document read
    /// back from the API can be written straight back. It is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub id: Option<String>,
    /// Raw identifier, accepted and ignored for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub uuid: Option<String>,
    /// The upstream this route belongs to.
    #[schema(value_type = String)]
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match rule; exactly one of `http` or `grpc`.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_config: MatchConfig,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy; overrides the upstream's when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cors: Option<CorsConfig>,
}

/// Replace a route. The upstream is immutable, so it is absent here.
#[derive(Clone)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RouteReplaceDto {
    /// Server-generated identifier, accepted and ignored so a document read
    /// back from the API can be written straight back. It is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub id: Option<String>,
    /// Raw identifier, accepted and ignored for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub uuid: Option<String>,
    /// The upstream a route belongs to is immutable, so a value supplied here
    /// is accepted and ignored rather than rejected; the stored one is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub upstream_id: Option<String>,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match rule; exactly one of `http` or `grpc`.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_config: MatchConfig,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy; overrides the upstream's when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cors: Option<CorsConfig>,
}

/// A route as returned by the management API.
#[derive(Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Anonymous global type system identifier.
    pub id: String,
    /// Raw identifier.
    #[schema(value_type = String)]
    pub uuid: Uuid,
    /// The upstream this route belongs to.
    #[schema(value_type = String)]
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Match rule.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_config: MatchConfig,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Plugin bindings.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub cors: Option<CorsConfig>,
}

/// A page of routes.
#[derive(Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteListDto {
    /// The page's items.
    pub items: Vec<RouteDto>,
    /// Number of items in this page.
    pub count: usize,
}

/// Create a custom plugin.
#[derive(Clone)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct PluginCreateDto {
    /// Server-generated identifier, accepted and ignored so a document read
    /// back from the API can be written straight back. It is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub id: Option<String>,
    /// Raw identifier, accepted and ignored for the same reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub uuid: Option<String>,
    /// Kind of plugin.
    #[schema(value_type = String)]
    pub plugin_type: PluginType,
    /// Unique name within the tenant.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Schema the plugin's configuration must satisfy.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config_schema: serde_json::Value,
    /// Phases a transform plugin participates in.
    #[serde(default)]
    #[schema(value_type = Vec<String>)]
    pub phases: Vec<TransformPhase>,
    /// Plugin source, stored verbatim and never executed in this build.
    #[serde(default)]
    pub source_code: String,
}

/// A plugin as returned by the management API.
#[derive(Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Anonymous global type system identifier.
    pub id: String,
    /// Raw identifier.
    #[schema(value_type = String)]
    pub uuid: Uuid,
    /// Kind of plugin.
    #[schema(value_type = String)]
    pub plugin_type: PluginType,
    /// Unique name within the tenant.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Schema the plugin's configuration must satisfy.
    #[schema(value_type = Object)]
    pub config_schema: serde_json::Value,
    /// Phases a transform plugin participates in.
    #[schema(value_type = Vec<String>)]
    pub phases: Vec<TransformPhase>,
    /// Plugin source, stored verbatim.
    pub source_code: String,
}

/// A page of plugins.
#[derive(Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginListDto {
    /// The page's items.
    pub items: Vec<PluginDto>,
    /// Number of items in this page.
    pub count: usize,
}

/// Query parameters accepted by the list endpoints.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ListQuery {
    /// Maximum number of results; defaults to 50 and is capped at 100.
    #[serde(rename = "$top")]
    pub top: Option<usize>,
    /// Offset into the result set.
    #[serde(rename = "$skip")]
    pub skip: Option<usize>,
    /// Filter expression.
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    /// Fields to return.
    #[serde(rename = "$select")]
    pub select: Option<String>,
    /// Sort order.
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
}

/// Default page size for the list endpoints.
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size for the list endpoints.
pub const MAX_TOP: usize = 100;

impl ListQuery {
    /// Effective page size, defaulted and clamped.
    #[must_use]
    pub fn effective_top(&self) -> usize {
        self.top.unwrap_or(DEFAULT_TOP).min(MAX_TOP)
    }

    /// Effective offset.
    #[must_use]
    pub fn effective_skip(&self) -> usize {
        self.skip.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_TOP, ListQuery, MAX_TOP};

    #[test]
    fn page_size_defaults_and_clamps() {
        let empty = ListQuery::default();
        assert_eq!(empty.effective_top(), DEFAULT_TOP);
        assert_eq!(empty.effective_skip(), 0);

        let large = ListQuery {
            top: Some(5_000),
            ..ListQuery::default()
        };
        assert_eq!(large.effective_top(), MAX_TOP);

        let small = ListQuery {
            top: Some(3),
            skip: Some(7),
            ..ListQuery::default()
        };
        assert_eq!(small.effective_top(), 3);
        assert_eq!(small.effective_skip(), 7);
    }
}
