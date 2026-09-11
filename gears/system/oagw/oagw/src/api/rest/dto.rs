//! Wire shapes for the OAGW management surface.
//!
//! The nested value objects — endpoints, rate limits, header transforms,
//! plugin bindings — are carried as the domain types themselves: their JSON is
//! the documented wire shape, so a second mirror would only add a conversion to
//! get wrong. The schema annotations name them as opaque objects, which is what
//! a configuration block is to a client.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::model::{
    Cors, Endpoint, HeaderTransform, LoadBalancing, PluginBinding, RateLimit, Route, Scheme,
    SharingMode, Upstream,
};
use crate::domain::plugin::PluginDescriptor;
use crate::domain::services::control_plane::{RouteSpec, UpstreamSpec};

/// The `bool` the data model defaults to: an operator who says nothing meant
/// "on". `#[serde(default)]` alone would read an absent field as `false`, which
/// would silently create disabled routes and bindings.
fn default_on() -> bool {
    true
}

/// A single upstream endpoint as the operator supplies it.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request, response)]
#[serde(default, deny_unknown_fields)]
pub struct EndpointDto {
    /// URI scheme.
    #[schema(value_type = String)]
    pub scheme: Scheme,
    /// DNS name, IPv4 or bracketed IPv6 literal.
    pub host: String,
    /// Port; defaults to the scheme's standard port.
    pub port: Option<u16>,
    /// Path prefix prepended to the forwarded path.
    pub path_prefix: String,
    /// Load-balancing weight, 1..=100.
    pub weight: u32,
    /// Selection priority; lower is preferred.
    pub priority: u32,
    /// Free-form metadata.
    pub metadata: std::collections::HashMap<String, String>,
}

impl From<Endpoint> for EndpointDto {
    fn from(value: Endpoint) -> Self {
        Self {
            scheme: value.scheme,
            host: value.host,
            port: value.port,
            path_prefix: value.path_prefix,
            weight: value.weight,
            priority: value.priority,
            metadata: value.metadata,
        }
    }
}

impl From<EndpointDto> for Endpoint {
    fn from(value: EndpointDto) -> Self {
        Self {
            scheme: value.scheme,
            host: value.host,
            port: value.port,
            path_prefix: value.path_prefix,
            weight: value.weight,
            priority: value.priority,
            metadata: value.metadata,
        }
    }
}

impl From<&Endpoint> for EndpointDto {
    fn from(value: &Endpoint) -> Self {
        Self::from(value.clone())
    }
}

impl EndpointDto {
    /// The default port the endpoint's scheme implies, used when no port is set.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// Whether the endpoint dials in plaintext.
    #[must_use]
    pub const fn is_plaintext(&self) -> bool {
        matches!(self.scheme, Scheme::Http)
    }
}

/// An upstream as it crosses the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Server-assigned identifier.
    pub id: Uuid,
    /// Routing key in `/oagw/v1/proxy/{alias}/...`; immutable once set.
    pub alias: String,
    /// Human label.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Endpoint pool.
    #[schema(value_type = Vec<EndpointDto>)]
    pub endpoints: Vec<EndpointDto>,
    /// Load-balancing strategy across the pool.
    #[schema(value_type = String)]
    pub load_balancing: LoadBalancing,
    /// Credential injection methods.
    #[schema(value_type = Vec<Object>)]
    pub auth_methods: Vec<crate::domain::model::AuthMethod>,
    /// Add-only label set.
    pub tags: Vec<String>,
    /// Visibility to descendant tenants.
    #[schema(value_type = String)]
    pub sharing: SharingMode,
    /// Rate limit for the pool.
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimit>,
    /// Whether the pool accepts traffic at all.
    #[schema(value_type = bool)]
    pub enabled: bool,
    /// Creation instant.
    #[schema(value_type = String)]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last update instant.
    #[schema(value_type = String)]
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<Upstream> for UpstreamDto {
    fn from(value: Upstream) -> Self {
        Self {
            id: value.id,
            alias: value.alias,
            name: value.name,
            description: value.description,
            tenant_id: value.tenant_id,
            endpoints: value.endpoints.into_iter().map(EndpointDto::from).collect(),
            load_balancing: value.load_balancing,
            auth_methods: value.auth_methods,
            tags: value.tags,
            sharing: value.sharing,
            rate_limit: value.rate_limit,
            enabled: value.enabled,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

impl From<UpstreamDto> for Upstream {
    fn from(value: UpstreamDto) -> Self {
        Self {
            id: value.id,
            alias: value.alias,
            name: value.name,
            description: value.description,
            tenant_id: value.tenant_id,
            endpoints: value.endpoints.into_iter().map(Endpoint::from).collect(),
            load_balancing: value.load_balancing,
            auth_methods: value.auth_methods,
            tags: value.tags,
            sharing: value.sharing,
            rate_limit: value.rate_limit,
            enabled: value.enabled,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Body of `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(default, deny_unknown_fields)]
pub struct UpstreamCreateRequest {
    /// Alias to register. Optional: derived from the pool when omitted.
    pub alias: Option<String>,
    /// Human label.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// Endpoint pool.
    #[schema(value_type = Vec<EndpointDto>)]
    pub endpoints: Vec<EndpointDto>,
    /// Load-balancing strategy across the pool.
    #[schema(value_type = String)]
    pub load_balancing: LoadBalancing,
    /// Credential injection methods.
    #[schema(value_type = Vec<Object>)]
    pub auth_methods: Vec<crate::domain::model::AuthMethod>,
    /// Add-only label set.
    pub tags: Vec<String>,
    /// Visibility to descendant tenants.
    #[schema(value_type = String)]
    pub sharing: SharingMode,
    /// Rate limit for the pool.
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimit>,
    /// Whether the pool accepts traffic at all; a pool that does not answers
    /// every proxy call with `503`.
    #[schema(value_type = Option<bool>)]
    pub enabled: Option<bool>,
}

impl UpstreamCreateRequest {
    /// The operator's view of the upstream, ready for the control plane.
    #[must_use]
    pub fn into_spec(self) -> UpstreamSpec {
        UpstreamSpec {
            alias: self.alias,
            name: self.name,
            description: self.description,
            endpoints: self.endpoints.into_iter().map(Endpoint::from).collect(),
            load_balancing: self.load_balancing,
            auth_methods: self.auth_methods,
            tags: self.tags,
            sharing: self.sharing,
            rate_limit: self.rate_limit,
            enabled: self.enabled.unwrap_or(true),
        }
    }
}

/// Page of upstreams as returned by `GET /oagw/v1/upstreams`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamListDto {
    /// The page's items.
    #[schema(value_type = Vec<UpstreamDto>)]
    pub items: Vec<serde_json::Value>,
    /// Number of items matching the query before paging.
    pub total_count: u64,
}

/// A route as it crosses the wire.
///
/// The three switches are independent on the wire, exactly as the API contract
/// spells them, so they stay three fields rather than a fold.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::struct_excessive_bools)]
pub struct RouteDto {
    /// Server-assigned identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Path pattern; starts with `/` and may contain `{param}` segments.
    pub path: String,
    /// Allowed methods, uppercase, or `*`.
    pub methods: Vec<String>,
    /// Upstream alias the route forwards to.
    pub target_alias: String,
    /// Prefix prepended to the forwarded path.
    pub target_path_prefix: String,
    /// Whether the matched prefix is stripped from the forwarded path.
    pub strip_prefix: bool,
    /// Whether the client's `Host` is forwarded.
    pub preserve_host: bool,
    /// Request header transformations.
    #[schema(value_type = Vec<Object>)]
    pub request_headers: Vec<HeaderTransform>,
    /// Response header transformations.
    #[schema(value_type = Vec<Object>)]
    pub response_headers: Vec<HeaderTransform>,
    /// Per-route timeout override in seconds.
    pub timeout_secs: Option<u64>,
    /// Per-route rate limit.
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimit>,
    /// Plugins bound to the route.
    #[schema(value_type = Vec<Object>)]
    pub plugins: Vec<PluginBinding>,
    /// CORS configuration.
    #[schema(value_type = Option<Object>)]
    pub cors: Option<Cors>,
    /// Ordering among equally specific matches; higher wins.
    pub priority: u32,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// What happens to the path beyond the route's own.
    #[schema(value_type = String)]
    pub path_suffix_mode: crate::domain::model::PathSuffixMode,
    /// Which of the caller's own headers the upstream may see.
    #[schema(value_type = String)]
    pub passthrough: crate::domain::model::Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
    /// Add-only label set.
    pub tags: Vec<String>,
    /// Creation instant.
    #[schema(value_type = String)]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last update instant.
    #[schema(value_type = String)]
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<Route> for RouteDto {
    fn from(value: Route) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id,
            path: value.path,
            methods: value.methods,
            target_alias: value.target_alias,
            target_path_prefix: value.target_path_prefix,
            strip_prefix: value.strip_prefix,
            preserve_host: value.preserve_host,
            request_headers: value.request_headers,
            response_headers: value.response_headers,
            timeout_secs: value.timeout_secs,
            rate_limit: value.rate_limit,
            plugins: value.plugins,
            cors: value.cors,
            priority: value.priority,
            enabled: value.enabled,
            path_suffix_mode: value.path_suffix_mode,
            passthrough: value.passthrough,
            passthrough_allowlist: value.passthrough_allowlist,
            tags: value.tags,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Body of `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`.
///
/// The switches are independent on the wire, exactly as the API contract spells
/// them, so they stay three fields rather than a fold.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub struct RouteCreateRequest {
    /// Path pattern.
    pub path: String,
    /// Allowed methods.
    pub methods: Vec<String>,
    /// Upstream alias to forward to.
    pub target_alias: String,
    /// Prefix prepended to the forwarded path.
    pub target_path_prefix: String,
    /// Whether the matched prefix is stripped from the forwarded path.
    #[serde(default = "default_on")]
    pub strip_prefix: bool,
    /// Whether the client's `Host` is forwarded.
    pub preserve_host: bool,
    /// Request header transformations.
    #[schema(value_type = Vec<Object>)]
    pub request_headers: Vec<HeaderTransform>,
    /// Response header transformations.
    #[schema(value_type = Vec<Object>)]
    pub response_headers: Vec<HeaderTransform>,
    /// Per-route timeout override in seconds.
    pub timeout_secs: Option<u64>,
    /// Per-route rate limit.
    #[schema(value_type = Option<Object>)]
    pub rate_limit: Option<RateLimit>,
    /// Plugins bound to the route.
    #[schema(value_type = Vec<Object>)]
    pub plugins: Vec<PluginBinding>,
    /// CORS configuration.
    #[schema(value_type = Option<Object>)]
    pub cors: Option<Cors>,
    /// Ordering among equally specific matches; higher wins.
    pub priority: u32,
    /// Whether the route participates in matching.
    #[serde(default = "default_on")]
    pub enabled: bool,
    /// What happens to the path beyond the route's own.
    #[schema(value_type = String)]
    #[serde(default)]
    pub path_suffix_mode: crate::domain::model::PathSuffixMode,
    /// Which of the caller's own headers the upstream may see.
    #[schema(value_type = String)]
    #[serde(default)]
    pub passthrough: crate::domain::model::Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
    /// Add-only label set.
    pub tags: Vec<String>,
}

impl RouteCreateRequest {
    /// The operator's view of the route, ready for the control plane.
    #[must_use]
    pub fn into_spec(self) -> RouteSpec {
        RouteSpec {
            path: self.path,
            methods: self.methods,
            target_alias: self.target_alias,
            target_path_prefix: self.target_path_prefix,
            strip_prefix: self.strip_prefix,
            preserve_host: self.preserve_host,
            request_headers: self.request_headers,
            response_headers: self.response_headers,
            timeout_secs: self.timeout_secs,
            rate_limit: self.rate_limit,
            plugins: self.plugins,
            cors: self.cors,
            priority: self.priority,
            enabled: self.enabled,
            path_suffix_mode: self.path_suffix_mode,
            passthrough: self.passthrough,
            passthrough_allowlist: self.passthrough_allowlist,
            tags: self.tags,
        }
    }
}

/// Page of routes as returned by `GET /oagw/v1/routes`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteListDto {
    /// The page's items.
    #[schema(value_type = Vec<RouteDto>)]
    pub items: Vec<serde_json::Value>,
    /// Number of items matching the query before paging.
    pub total_count: u64,
}

/// A catalog entry as returned by `GET /oagw/v1/plugins`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Plugin identifier.
    pub id: String,
    /// Class the plugin belongs to.
    #[schema(value_type = String)]
    pub plugin_type: crate::domain::plugin::PluginType,
    /// Implementation version.
    pub version: String,
    /// Operator-facing description.
    pub description: String,
    /// Whether this build ships an implementation.
    pub built_in: bool,
}

impl From<PluginDescriptor> for PluginDto {
    fn from(value: PluginDescriptor) -> Self {
        Self {
            id: value.id,
            plugin_type: value.plugin_type,
            version: value.version,
            description: value.description,
            built_in: value.built_in,
        }
    }
}

/// Page of plugins as returned by `GET /oagw/v1/plugins`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginListDto {
    /// The page's items.
    #[schema(value_type = Vec<PluginDto>)]
    pub items: Vec<PluginDto>,
    /// Number of entries in the catalog.
    pub total_count: u64,
}
