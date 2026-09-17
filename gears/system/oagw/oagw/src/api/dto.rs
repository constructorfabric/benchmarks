//! REST DTOs for the OAGW management API.
//!
//! Wire shapes follow `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`. `http` is additionally accepted as an
//! endpoint scheme when `oagw.config.allow_http_upstream` is enabled.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeaderRules, MatchRule, PluginBinding, PluginsConfig,
    RateLimit, ServerConfig,
};

/// Request body for `POST /oagw/v1/upstreams`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CreateUpstreamRequest {
    /// Explicit alias; required for IP-based or non-derivable endpoints.
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic (default true).
    pub enabled: Option<bool>,
    /// Endpoint pool.
    pub server: Option<ServerConfigDto>,
    /// Upstream protocol GTS identifier.
    pub protocol: Option<String>,
    /// Flat tags.
    pub tags: Option<Vec<String>>,
    /// Header transformation rules.
    pub headers: Option<HeaderRules>,
    /// Upstream-level rate limit.
    pub rate_limit: Option<RateLimit>,
    /// Upstream-level CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Outbound auth configuration.
    pub auth: Option<AuthConfig>,
    /// Plugin chain.
    pub plugins: Option<UpstreamPluginsDto>,
}

/// Upstream plugin list on the wire (items may be strings or objects).
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpstreamPluginsDto {
    /// Sharing mode.
    pub sharing: Option<crate::domain::model::SharingMode>,
    /// Ordered plugin references.
    pub items: Option<Vec<Value>>,
}

/// Request body for `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpdateUpstreamRequest {
    /// Alias (immutable; tolerated when equal to the existing value).
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    pub enabled: Option<bool>,
    /// Endpoint pool.
    pub server: Option<ServerConfig>,
    /// Upstream protocol GTS identifier.
    pub protocol: Option<String>,
    /// Flat tags.
    pub tags: Option<Vec<String>>,
    /// Header transformation rules.
    pub headers: Option<HeaderRules>,
    /// Upstream-level rate limit.
    pub rate_limit: Option<RateLimit>,
    /// Upstream-level CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Outbound auth configuration.
    pub auth: Option<AuthConfig>,
    /// Plugin chain.
    pub plugins: Option<UpstreamPluginsDto>,
}

/// Upstream resource response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing identifier (lowercase, unique per tenant).
    pub alias: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol GTS identifier.
    pub protocol: String,
    /// Flat tags.
    pub tags: Vec<String>,
    /// Header transformation rules.
    pub headers: HeaderRules,
    /// Upstream-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Upstream-level CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Outbound auth configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last modification timestamp (RFC 3339).
    pub updated_at: String,
}

/// Parse wire plugin items (`string | object`) into bindings.
#[must_use]
pub fn parse_plugin_items(items: Option<Vec<Value>>) -> PluginsConfig {
    let mut config = PluginsConfig::default();
    for value in items.unwrap_or_default() {
        if let Some(binding) = PluginsConfig::parse_item(&value) {
            config.items.push(binding);
        }
    }
    config
}

/// Render a plugin list back onto the wire as plain reference strings.
#[must_use]
pub fn render_plugin_items(items: &[PluginBinding]) -> Vec<Value> {
    items
        .iter()
        .map(|b| Value::String(b.plugin_ref.clone()))
        .collect()
}

impl UpstreamPluginsDto {
    /// Convert to the domain representation.
    #[must_use]
    pub fn into_domain(self) -> PluginsConfig {
        PluginsConfig {
            sharing: self.sharing.unwrap_or_default(),
            items: parse_plugin_items(self.items).items,
        }
    }
}

impl From<PluginsConfig> for UpstreamPluginsDto {
    fn from(config: PluginsConfig) -> Self {
        Self {
            sharing: Some(config.sharing),
            items: Some(render_plugin_items(&config.items)),
        }
    }
}

/// Request body for `POST /oagw/v1/routes`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CreateRouteRequest {
    /// Referenced upstream (must belong to the calling tenant).
    pub upstream_id: Option<Uuid>,
    /// Flat tags.
    pub tags: Option<Vec<String>>,
    /// Protocol-scoped match rule.
    pub r#match: Option<MatchRule>,
    /// Plugin chain.
    pub plugins: Option<UpstreamPluginsDto>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimit>,
    /// Route-level CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Route-level header rules.
    pub headers: Option<HeaderRules>,
}

/// Request body for `PUT /oagw/v1/routes/{id}`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpdateRouteRequest {
    /// Flat tags.
    pub tags: Option<Vec<String>>,
    /// Protocol-scoped match rule.
    pub r#match: Option<MatchRule>,
    /// Plugin chain.
    pub plugins: Option<UpstreamPluginsDto>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimit>,
    /// Route-level CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Route-level header rules.
    pub headers: Option<HeaderRules>,
}

/// Route resource response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Referenced upstream.
    pub upstream_id: Uuid,
    /// Flat tags.
    pub tags: Vec<String>,
    /// Protocol-scoped match rule.
    pub r#match: MatchRule,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Route-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Route-level CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level header rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderRules>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last modification timestamp (RFC 3339).
    pub updated_at: String,
}

/// Request body for `POST /oagw/v1/plugins`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CreatePluginRequest {
    /// `auth_plugin` / `guard_plugin` / `transform_plugin`.
    pub plugin_type: Option<String>,
    /// Human-readable name.
    pub name: Option<String>,
    /// Configuration schema (JSON Schema object).
    pub config_schema: Option<Value>,
    /// Plugin source code.
    pub source_code: Option<String>,
}

/// Plugin resource response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// System-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin type fragment.
    pub plugin_type: String,
    /// Human-readable name.
    pub name: String,
    /// Configuration schema.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Plugin source code.
    pub source_code: String,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last-use timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// GC-eligibility timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<String>,
}

/// Server endpoint pool (re-exported so OpenAPI picks up the schema).
pub type ServerConfigDto = ServerConfig;

/// Endpoint definition (re-exported so OpenAPI picks up the schema).
pub type EndpointDto = Endpoint;

impl From<crate::domain::model::Upstream> for UpstreamDto {
    fn from(u: crate::domain::model::Upstream) -> Self {
        Self {
            id: u.id,
            tenant_id: u.tenant_id,
            alias: u.alias,
            enabled: u.enabled,
            server: u.server,
            protocol: u.protocol,
            tags: u.tags,
            headers: u.headers,
            rate_limit: u.rate_limit,
            cors: u.cors,
            auth: u.auth,
            plugins: u.plugins,
            created_at: u.created_at,
            updated_at: u.updated_at,
        }
    }
}

impl From<crate::domain::model::Route> for RouteDto {
    fn from(r: crate::domain::model::Route) -> Self {
        Self {
            id: r.id,
            tenant_id: r.tenant_id,
            upstream_id: r.upstream_id,
            tags: r.tags,
            r#match: r.r#match,
            plugins: r.plugins,
            rate_limit: r.rate_limit,
            cors: r.cors,
            headers: r.headers,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

impl From<crate::domain::model::Plugin> for PluginDto {
    fn from(p: crate::domain::model::Plugin) -> Self {
        Self {
            id: p.id,
            tenant_id: p.tenant_id,
            plugin_type: p.plugin_type,
            name: p.name,
            config_schema: p.config_schema,
            source_code: p.source_code,
            created_at: p.created_at,
            last_used_at: p.last_used_at,
            gc_eligible_at: p.gc_eligible_at,
        }
    }
}
