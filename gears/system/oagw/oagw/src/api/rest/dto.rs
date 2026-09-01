//! REST DTOs for the OAGW management API.
//!
//! The request shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` field for field (`deny_unknown_fields`,
//! `snake_case`, schema defaults). The configuration sub-structures are
//! re-used from `crate::domain::models` so the domain and the wire cannot
//! drift apart.
//!
//! Resource identifiers in paths are anonymous GTS ids
//! (`gts.cf.core.oagw.{type}.v1~{uuid}`); both the full id and the bare UUID
//! are accepted in paths, and bodies always return the full GTS id.

use uuid::Uuid;

use crate::domain::models::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginKind, PluginPhase, Protocol,
    RateLimitConfig, ServerConfig,
};

/// Wire representation of a stored upstream.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Anonymous GTS identifier: `gts.cf.core.oagw.upstream.v1~{uuid}`.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key, unique per tenant.
    pub alias: String,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Authentication plugin binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfigDto>,
    /// Upstream-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Discovery tags.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation time (RFC 3339 UTC).
    pub created_at: String,
    /// Last modification time (RFC 3339 UTC).
    pub updated_at: String,
}

/// Wire representation of the plugin chain of an upstream or route.
#[derive(Debug, Clone, Default, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfigDto {
    /// Sharing mode for hierarchical inheritance.
    #[serde(default)]
    pub sharing: crate::domain::models::SharingMode,
    /// Built-in plugins by GTS id, custom plugins by full GTS id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// Wire representation of a stored route.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Anonymous GTS identifier: `gts.cf.core.oagw.route.v1~{uuid}`.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Referenced upstream, as a full GTS id.
    pub upstream_id: String,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Sort key for deterministic matching.
    pub priority: i32,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Route-level plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfigDto>,
    /// Route-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Discovery tags.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation time (RFC 3339 UTC).
    pub created_at: String,
    /// Last modification time (RFC 3339 UTC).
    pub updated_at: String,
}

/// Payload of `POST /upstreams`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateUpstreamRequest {
    /// Operator-supplied alias; derived from the endpoints when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts proxy traffic; defaults to `true`.
    #[serde(default = "crate::api::rest::defaults::default_true")]
    pub enabled: bool,
    /// Upstream protocol; defaults to the HTTP protocol id.
    #[serde(default = "crate::api::rest::defaults::default_protocol")]
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfigDto>,
    /// Upstream-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Payload of `PUT /upstreams/{id}` — full replacement.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct ReplaceUpstreamRequest {
    /// Operator-supplied alias; derived from the endpoints when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts proxy traffic; defaults to `true`.
    #[serde(default = "crate::api::rest::defaults::default_true")]
    pub enabled: bool,
    /// Upstream protocol.
    #[serde(default = "crate::api::rest::defaults::default_protocol")]
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfigDto>,
    /// Upstream-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Discovery tags (replaced wholesale).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Deserializes a resource reference that may be spelled either as the bare
/// UUID or as the anonymous GTS id (`gts.cf.core.oagw.upstream.v1~{uuid}`).
///
/// Wire bodies always *return* the full GTS id; accepting the bare UUID here
/// keeps the schema's `format: uuid` usable for callers that already hold it.
fn deserialize_upstream_ref<'de, D>(deserializer: D) -> Result<Uuid, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <String as serde::Deserialize>::deserialize(deserializer)?;
    crate::domain::models::strip_gts_prefix(crate::domain::models::UPSTREAM_TYPE, &raw)
        .unwrap_or(raw.as_str())
        .parse::<Uuid>()
        .map_err(|_| {
            serde::de::Error::custom(format!(
                "upstream_id must be a UUID or 'gts.cf.core.oagw.upstream.v1~<uuid>', got '{raw}'"
            ))
        })
}

/// Payload of `POST /routes`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateRouteRequest {
    /// Referenced upstream (bare UUID or full GTS id).
    #[serde(deserialize_with = "deserialize_upstream_ref")]
    pub upstream_id: Uuid,
    /// Whether the route participates in matching; defaults to `true`.
    #[serde(default = "crate::api::rest::defaults::default_true")]
    pub enabled: bool,
    /// Sort key for deterministic matching; defaults to `0`.
    #[serde(default)]
    pub priority: i32,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfigDto>,
    /// Route-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Payload of `PUT /routes/{id}` — `upstream_id` is immutable and absent.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct ReplaceRouteRequest {
    /// Whether the route participates in matching; defaults to `true`.
    #[serde(default = "crate::api::rest::defaults::default_true")]
    pub enabled: bool,
    /// Sort key for deterministic matching; defaults to `0`.
    #[serde(default)]
    pub priority: i32,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfigDto>,
    /// Route-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Discovery tags (replaced wholesale).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Payload of `POST /plugins`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreatePluginRequest {
    /// Plugin family; `type` is accepted as an alias of `plugin_type`.
    #[serde(alias = "type")]
    pub plugin_type: PluginKind,
    /// Unique name within the tenant.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Phases implemented by a transform plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PluginPhase>,
    /// JSON Schema validating the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Sandboxed Starlark source.
    pub source_code: String,
}

/// Wire representation of a stored custom plugin.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Anonymous GTS identifier: `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin family.
    #[serde(alias = "type")]
    pub plugin_type: PluginKind,
    /// Unique name within the tenant.
    pub name: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Phases implemented by the plugin.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PluginPhase>,
    /// JSON Schema validating the plugin configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Creation time (RFC 3339 UTC).
    pub created_at: String,
    /// Last modification time (RFC 3339 UTC).
    pub updated_at: String,
}

/// Response of `GET /plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// Anonymous GTS identifier of the plugin.
    pub id: String,
    /// Plugin family.
    #[serde(alias = "type")]
    pub plugin_type: PluginKind,
    /// Unique name within the tenant.
    pub name: String,
    /// Sandboxed Starlark source.
    pub source_code: String,
}

impl From<crate::domain::models::PluginsConfig> for PluginsConfigDto {
    fn from(config: crate::domain::models::PluginsConfig) -> Self {
        Self {
            sharing: config.sharing,
            items: config.items,
        }
    }
}

impl PluginsConfigDto {
    /// Converts the wire shape into the domain shape.
    #[must_use]
    pub fn into_domain(self) -> crate::domain::models::PluginsConfig {
        crate::domain::models::PluginsConfig {
            sharing: self.sharing,
            items: self.items,
        }
    }
}

impl From<crate::domain::models::Upstream> for UpstreamDto {
    fn from(upstream: crate::domain::models::Upstream) -> Self {
        Self {
            id: upstream.gts_id(),
            tenant_id: upstream.tenant_id,
            alias: upstream.alias,
            enabled: upstream.enabled,
            protocol: upstream.protocol,
            server: upstream.server,
            auth: upstream.auth,
            headers: upstream.headers,
            plugins: upstream.plugins.map(PluginsConfigDto::from),
            rate_limit: upstream.rate_limit,
            cors: upstream.cors,
            tags: upstream.tags,
            created_at: crate::domain::time::format_epoch_millis(upstream.created_at),
            updated_at: crate::domain::time::format_epoch_millis(upstream.updated_at),
        }
    }
}

impl From<crate::domain::models::Route> for RouteDto {
    fn from(route: crate::domain::models::Route) -> Self {
        Self {
            id: route.gts_id(),
            tenant_id: route.tenant_id,
            // Bodies carry the anonymous GTS id, never the bare UUID.
            upstream_id: crate::domain::models::gts_instance_id(
                crate::domain::models::UPSTREAM_TYPE,
                route.upstream_id,
            ),
            enabled: route.enabled,
            priority: route.priority,
            match_config: route.match_config,
            plugins: route.plugins.map(PluginsConfigDto::from),
            rate_limit: route.rate_limit,
            tags: route.tags,
            created_at: crate::domain::time::format_epoch_millis(route.created_at),
            updated_at: crate::domain::time::format_epoch_millis(route.updated_at),
        }
    }
}

impl From<crate::domain::models::Plugin> for PluginDto {
    fn from(plugin: crate::domain::models::Plugin) -> Self {
        Self {
            id: plugin.gts_id(),
            tenant_id: plugin.tenant_id,
            plugin_type: plugin.plugin_type,
            name: plugin.name,
            description: plugin.description,
            phases: plugin.phases,
            config_schema: plugin.config_schema,
            created_at: crate::domain::time::format_epoch_millis(plugin.created_at),
            updated_at: crate::domain::time::format_epoch_millis(plugin.updated_at),
        }
    }
}

// Re-exported so the OpenAPI schema set of the gear is self-contained: the
// request/response DTOs embed these structures.
pub use crate::domain::models::{
    Endpoint as EndpointSchema, GrpcMatch as GrpcMatchSchema, HttpMethod as HttpMethodSchema,
    HttpMatch as HttpMatchSchema, RateLimitAlgorithm as RateLimitAlgorithmSchema,
    RateLimitScope as RateLimitScopeSchema, RateLimitStrategy as RateLimitStrategySchema,
    RateWindow as RateWindowSchema, SharingMode as SharingModeSchema,
    SustainedRate as SustainedRateSchema, BurstCapacity as BurstCapacitySchema,
    HeaderRules as HeaderRulesSchema, HeaderPassthrough as HeaderPassthroughSchema,
    HeadersConfig as HeadersConfigSchema, PluginsConfig as PluginsConfigSchema,
    PathSuffixMode as PathSuffixModeSchema, CorsMethod as CorsMethodSchema,
};
