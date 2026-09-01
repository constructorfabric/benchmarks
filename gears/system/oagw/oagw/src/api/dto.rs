// Created: 2026-08-31 by Constructor Tech
//! REST DTOs for the OAGW management API (DESIGN §3.3).
//!
//! Response bodies carry the **bare UUID** in `id`; timestamps are epoch
//! milliseconds. Every nested configuration member is a shared
//! [`crate::domain::model`] type, so the wire shape and the domain shape are
//! the same type.

use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, Plugin, PluginKind, PluginsConfig, Protocol,
    RateLimitConfig, Route, RouteMatch, Upstream,
};
use crate::domain::spec::{
    EndpointSpec, PluginSpec, RouteSpec, RouteUpdateSpec, ServerSpec, UpstreamSpec,
};

/// REST DTO for an upstream resource.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key, unique per tenant.
    pub alias: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Wire protocol (canonical GTS id).
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerSpec,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation instant (epoch milliseconds).
    pub created_at: u64,
    /// Last modification instant (epoch milliseconds).
    pub updated_at: u64,
}

impl From<Upstream> for UpstreamDto {
    fn from(record: Upstream) -> Self {
        Self {
            id: record.id,
            tenant_id: record.tenant_id,
            alias: record.alias,
            enabled: record.enabled,
            protocol: record.protocol,
            server: ServerSpec {
                endpoints: record
                    .endpoints
                    .into_iter()
                    .map(EndpointSpec::from)
                    .collect(),
            },
            tags: record.tags,
            auth: record.auth,
            headers: record.headers,
            plugins: record.plugins,
            rate_limit: record.rate_limit,
            cors: record.cors,
            created_at: record.timestamps.created_at,
            updated_at: record.timestamps.updated_at,
        }
    }
}

/// REST DTO for creating an upstream (POST /upstreams).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateUpstreamRequest {
    /// Explicit alias; derived from the endpoints when omitted. Rejected when
    /// the endpoints imply a different value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic; defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Wire protocol (canonical GTS id).
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerSpec,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<CreateUpstreamRequest> for UpstreamSpec {
    fn from(request: CreateUpstreamRequest) -> Self {
        Self {
            alias: request.alias,
            enabled: request.enabled,
            protocol: request.protocol,
            server: request.server,
            tags: request.tags,
            auth: request.auth,
            headers: request.headers,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

/// REST DTO for replacing an upstream (PUT /upstreams/{id}).
///
/// The alias is **immutable** for hostname pools; an IP-based upstream may
/// repeat its current alias. `id` and `tenant_id` are never part of the
/// payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateUpstreamRequest {
    /// Current alias (hostname pools reject any other value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic; defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Wire protocol (canonical GTS id).
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerSpec,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<UpdateUpstreamRequest> for UpstreamSpec {
    fn from(request: UpdateUpstreamRequest) -> Self {
        Self {
            alias: request.alias,
            enabled: request.enabled,
            protocol: request.protocol,
            server: request.server,
            tags: request.tags,
            auth: request.auth,
            headers: request.headers,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

/// REST DTO for a route resource.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Match rule (`match` on the wire).
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy (ADR-0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation instant (epoch milliseconds).
    pub created_at: u64,
    /// Last modification instant (epoch milliseconds).
    pub updated_at: u64,
}

impl From<Route> for RouteDto {
    fn from(record: Route) -> Self {
        Self {
            id: record.id,
            tenant_id: record.tenant_id,
            upstream_id: record.upstream_id,
            enabled: record.enabled,
            match_rule: record.match_rule,
            tags: record.tags,
            plugins: record.plugins,
            rate_limit: record.rate_limit,
            cors: record.cors,
            created_at: record.timestamps.created_at,
            updated_at: record.timestamps.updated_at,
        }
    }
}

/// REST DTO for creating a route (POST /routes).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateRouteRequest {
    /// Owning upstream; must belong to the calling tenant.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching; defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Match rule (`match` on the wire).
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy (ADR-0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<CreateRouteRequest> for RouteSpec {
    fn from(request: CreateRouteRequest) -> Self {
        Self {
            upstream_id: request.upstream_id,
            enabled: request.enabled,
            match_rule: request.match_rule,
            tags: request.tags,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

/// REST DTO for replacing a route (PUT /routes/{id}).
///
/// `upstream_id` is **immutable** and therefore absent: moving a route to
/// another upstream means delete + create.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateRouteRequest {
    /// Whether the route participates in matching; defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Match rule (`match` on the wire).
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy (ADR-0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// REST DTO for a custom plugin resource (ADR-0002 appendix A).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Unique name within the tenant.
    pub name: String,
    /// Plugin family (`auth` | `guard` | `transform`).
    pub plugin_type: PluginKind,
    /// Whether the plugin is enabled.
    pub enabled: bool,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Plugin configuration.
    pub config: serde_json::Value,
    /// JSON Schema describing `config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source.
    pub source_code: String,
    /// Creation instant (epoch milliseconds).
    pub created_at: u64,
    /// Last modification instant (epoch milliseconds).
    pub updated_at: u64,
}

impl From<Plugin> for PluginDto {
    fn from(record: Plugin) -> Self {
        Self {
            id: record.id,
            tenant_id: record.tenant_id,
            name: record.name,
            plugin_type: record.kind,
            enabled: record.enabled,
            description: record.description,
            config: record.config,
            config_schema: record.config_schema,
            source_code: record.source,
            created_at: record.timestamps.created_at,
            updated_at: record.timestamps.updated_at,
        }
    }
}

/// REST DTO for creating a plugin (POST /plugins).
///
/// Plugins are immutable after creation: there is no PUT.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreatePluginRequest {
    /// Unique name within the tenant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Plugin family (`auth` | `guard` | `transform`).
    #[serde(rename = "plugin_type", alias = "type", default)]
    pub plugin_type: Option<PluginKind>,
    /// Whether the plugin is enabled; defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// JSON Schema describing `config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Starlark source.
    #[serde(alias = "source", default)]
    pub source_code: Option<String>,
}

impl From<UpdateRouteRequest> for RouteUpdateSpec {
    fn from(request: UpdateRouteRequest) -> Self {
        Self {
            enabled: request.enabled,
            match_rule: request.match_rule,
            tags: request.tags,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

impl From<CreatePluginRequest> for PluginSpec {
    fn from(request: CreatePluginRequest) -> Self {
        Self {
            name: request.name,
            plugin_type: request.plugin_type,
            enabled: request.enabled,
            config: request.config,
            config_schema: request.config_schema,
            description: request.description,
            source_code: request.source_code,
        }
    }
}
