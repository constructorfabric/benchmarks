//! REST DTOs for the OAGW management plane.
//!
//! Resource payloads speak bare UUIDs (`docs/schemas/upstream.v1.schema.json`
//! uses `format: uuid`); the policy blocks (`headers`, `rate_limit`, `cors`)
//! cross the wire as JSON objects and are validated by the domain model, so
//! they are carried opaquely here.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model;
use crate::domain::services::{PluginSpec, RouteSpec, UpstreamSpec};

// ---------------------------------------------------------------------------
// Shared blocks
// ---------------------------------------------------------------------------

/// Per-tenant sharing mode of a configuration block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum OagwSharingModeDto {
    /// The block applies only to the owning tenant.
    #[default]
    Private,
    /// Descendant tenants inherit the block.
    Inherit,
    /// Descendant tenants may not override the block.
    Enforce,
}

impl From<model::SharingMode> for OagwSharingModeDto {
    fn from(value: model::SharingMode) -> Self {
        match value {
            model::SharingMode::Private => Self::Private,
            model::SharingMode::Inherit => Self::Inherit,
            model::SharingMode::Enforce => Self::Enforce,
        }
    }
}

impl From<OagwSharingModeDto> for model::SharingMode {
    fn from(value: OagwSharingModeDto) -> Self {
        match value {
            OagwSharingModeDto::Private => model::SharingMode::Private,
            OagwSharingModeDto::Inherit => model::SharingMode::Inherit,
            OagwSharingModeDto::Enforce => model::SharingMode::Enforce,
        }
    }
}

/// Wire transport of a pooled endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum EndpointSchemeDto {
    /// Plaintext HTTP. Legal as an `endpoint.scheme`; whether plaintext is
    /// actually produced is governed by `oagw.config.allow_http_upstream`.
    Http,
    /// TLS-terminated HTTP.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport over TLS.
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl From<model::EndpointScheme> for EndpointSchemeDto {
    fn from(value: model::EndpointScheme) -> Self {
        match value {
            model::EndpointScheme::Http => Self::Http,
            model::EndpointScheme::Https => Self::Https,
            model::EndpointScheme::Wss => Self::Wss,
            model::EndpointScheme::Wt => Self::Wt,
            model::EndpointScheme::Grpc => Self::Grpc,
        }
    }
}

impl From<EndpointSchemeDto> for model::EndpointScheme {
    fn from(value: EndpointSchemeDto) -> Self {
        match value {
            EndpointSchemeDto::Http => model::EndpointScheme::Http,
            EndpointSchemeDto::Https => model::EndpointScheme::Https,
            EndpointSchemeDto::Wss => model::EndpointScheme::Wss,
            EndpointSchemeDto::Wt => model::EndpointScheme::Wt,
            EndpointSchemeDto::Grpc => model::EndpointScheme::Grpc,
        }
    }
}

/// One pooled endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct EndpointDto {
    /// `http`, `https`, `wss`, `wt` or `grpc`.
    #[serde(default)]
    pub scheme: EndpointSchemeDto,
    /// DNS name, IPv4 or IPv6 literal.
    pub host: String,
    /// Port; defaults to the scheme's standard port (80 for `http`, 443
    /// otherwise).
    #[serde(default)]
    pub port: Option<u16>,
}

impl TryFrom<&EndpointDto> for model::Endpoint {
    type Error = DomainError;

    fn try_from(value: &EndpointDto) -> Result<Self, Self::Error> {
        model::Endpoint::new(value.scheme.into(), &value.host, value.port)
    }
}

impl From<&model::Endpoint> for EndpointDto {
    fn from(value: &model::Endpoint) -> Self {
        Self {
            scheme: value.scheme.into(),
            host: value.host.clone(),
            port: Some(value.port),
        }
    }
}

/// Endpoint pool of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct EndpointPoolDto {
    /// At least one endpoint is required.
    pub endpoints: Vec<EndpointDto>,
}

impl TryFrom<&EndpointPoolDto> for model::ServerConfig {
    type Error = DomainError;

    fn try_from(value: &EndpointPoolDto) -> Result<Self, Self::Error> {
        let mut endpoints = Vec::with_capacity(value.endpoints.len());
        for endpoint in &value.endpoints {
            endpoints.push(model::Endpoint::try_from(endpoint)?);
        }
        Ok(model::ServerConfig { endpoints })
    }
}

impl From<&model::ServerConfig> for EndpointPoolDto {
    fn from(value: &model::ServerConfig) -> Self {
        Self {
            endpoints: value.endpoints.iter().map(EndpointDto::from).collect(),
        }
    }
}

/// Authentication plugin of an upstream.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct AuthDto {
    /// Full GTS id of the auth plugin.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Sharing of the auth configuration with descendant tenants.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Plugin configuration object.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

impl From<&model::AuthConfig> for AuthDto {
    fn from(value: &model::AuthConfig) -> Self {
        Self {
            plugin_type: value.plugin_type.clone(),
            sharing: value.sharing.into(),
            config: value.config.clone(),
        }
    }
}

/// One plugin reference of a chain.
///
/// The schemas spell a reference as a bare identifier string
/// (`docs/schemas/upstream.v1.schema.json` and `route.v1.schema.json` both
/// type `plugins.items[]` as strings); `docs/ADR/0009` also shows the bound
/// form that carries a plugin's per-binding configuration. Both are accepted.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(untagged)]
pub enum PluginRefDto {
    /// A bare reference: a built-in GTS id, or a custom plugin UUID.
    Bare(String),
    /// A reference with the configuration the binding runs with.
    Bound {
        /// Full GTS id of the plugin, or a custom plugin's anonymous id.
        plugin_ref: String,
        /// Resolved plugin UUID when the reference is a custom plugin row.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugin_uuid: Option<Uuid>,
        /// Configuration document the plugin runs with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<serde_json::Value>,
    },
}

impl PluginRefDto {
    /// The referenced plugin's identifier, in either spelling.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Bare(plugin_ref) => plugin_ref,
            Self::Bound { plugin_ref, .. } => plugin_ref,
        }
    }
}

impl From<&model::PluginBinding> for PluginRefDto {
    fn from(value: &model::PluginBinding) -> Self {
        match (&value.config, &value.plugin_uuid) {
            (None, None) => Self::Bare(value.plugin_ref.clone()),
            _ => Self::Bound {
                plugin_ref: value.plugin_ref.clone(),
                plugin_uuid: value.plugin_uuid,
                config: value.config.clone(),
            },
        }
    }
}

impl From<&PluginRefDto> for model::PluginBinding {
    fn from(value: &PluginRefDto) -> Self {
        match value {
            PluginRefDto::Bare(plugin_ref) => model::PluginBinding::bare(plugin_ref.clone()),
            PluginRefDto::Bound {
                plugin_ref,
                plugin_uuid,
                config,
            } => model::PluginBinding {
                plugin_ref: plugin_ref.clone(),
                plugin_uuid: *plugin_uuid,
                config: config.clone(),
            },
        }
    }
}

/// Ordered plugin chain of an upstream or a route.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct PluginChainDto {
    /// Sharing of the chain with descendant tenants.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Ordered plugin references; built-ins by full GTS id, custom plugins by
    /// UUID (both are canonicalized to the GTS id in responses).
    #[serde(default)]
    pub items: Vec<PluginRefDto>,
}

impl From<&model::PluginsConfig> for PluginChainDto {
    fn from(value: &model::PluginsConfig) -> Self {
        Self {
            sharing: value.sharing.into(),
            items: value.items.iter().map(PluginRefDto::from).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Response body for an upstream.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key, unique per tenant.
    pub alias: String,
    /// Disabled upstreams are not matched by the proxy.
    pub enabled: bool,
    /// Lower-case discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: EndpointPoolDto,
    /// Wire protocol GTS id.
    pub protocol: String,
    /// Authentication plugin, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthDto>,
    /// Header rewriting rules, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<serde_json::Value>,
    /// Plugin chain, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Rate limit, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<serde_json::Value>,
    /// CORS policy, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<serde_json::Value>,
    /// Creation instant, unix epoch milliseconds.
    pub created_at: u64,
    /// Last mutation instant, unix epoch milliseconds.
    pub updated_at: u64,
}

impl UpstreamDto {
    /// Project a domain upstream onto the wire.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when a policy block cannot be serialized.
    pub fn from_entity(value: &model::Upstream) -> Result<Self, DomainError> {
        Ok(Self {
            id: value.id,
            tenant_id: value.tenant_id,
            alias: value.alias.clone(),
            enabled: value.enabled,
            tags: value.tags.clone(),
            server: EndpointPoolDto::from(&value.server),
            protocol: value.protocol.gts_id().to_owned(),
            auth: value.auth.as_ref().map(AuthDto::from),
            headers: crate::api::rest::params::encode_policy(&value.headers)?,
            plugins: value.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: crate::api::rest::params::encode_policy(&value.rate_limit)?,
            cors: crate::api::rest::params::encode_policy(&value.cors)?,
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

/// Request body for `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
///
/// `id` and `tenant_id` are immutable: echoing them unchanged is tolerated,
/// a different value is rejected.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    /// Immutable resource id; tolerated when it echoes the stored value.
    pub id: Option<Uuid>,
    /// Immutable owning tenant; tolerated when it echoes the caller's tenant.
    pub tenant_id: Option<Uuid>,
    /// Caller-supplied alias; derived from the endpoints when omitted.
    pub alias: Option<String>,
    /// Defaults to `true`.
    pub enabled: Option<bool>,
    /// Cleared when omitted on a replace.
    pub tags: Option<Vec<String>>,
    /// Endpoint pool (required).
    pub server: Option<EndpointPoolDto>,
    /// Wire protocol GTS id (required).
    pub protocol: Option<String>,
    /// Authentication plugin.
    pub auth: Option<AuthDto>,
    /// Header rewriting rules; cleared when omitted.
    pub headers: Option<serde_json::Value>,
    /// Plugin chain; cleared when omitted.
    pub plugins: Option<PluginChainDto>,
    /// Rate limit; cleared when omitted.
    pub rate_limit: Option<serde_json::Value>,
    /// CORS policy; cleared when omitted.
    pub cors: Option<serde_json::Value>,
}

impl UpstreamRequest {
    /// Convert the request into a domain specification.
    ///
    /// # Errors
    ///
    /// [`DomainError`] for an unknown protocol or a malformed policy block.
    pub fn into_spec(self) -> Result<UpstreamSpec, DomainError> {
        let server = match &self.server {
            Some(pool) => Some(model::ServerConfig::try_from(pool)?),
            None => None,
        };
        let protocol = match self.protocol.as_deref() {
            Some(raw) => Some(model::Protocol::parse(raw)?),
            None => None,
        };
        Ok(UpstreamSpec {
            alias: self.alias,
            enabled: self.enabled,
            tags: self.tags,
            server,
            protocol,
            auth: self.auth.map(|auth| model::AuthConfig {
                plugin_type: auth.plugin_type,
                plugin_uuid: None,
                sharing: auth.sharing.into(),
                config: auth.config,
            }),
            headers: crate::api::rest::params::decode_policy("headers", &self.headers)?,
            plugins: self.plugins.map(|chain| model::PluginsConfig {
                sharing: chain.sharing.into(),
                items: chain.items.iter().map(model::PluginBinding::from).collect(),
            }),
            rate_limit: crate::api::rest::params::decode_policy("rate_limit", &self.rate_limit)?,
            cors: crate::api::rest::params::decode_policy("cors", &self.cors)?,
        })
    }
}

/// Paginated upstream list.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamListDto {
    /// One page of upstreams.
    pub items: Vec<UpstreamDto>,
    /// Number of upstreams matching the filter, before paging.
    pub total: usize,
    /// Page size applied.
    pub top: usize,
    /// Page offset applied.
    pub skip: usize,
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// HTTP matching rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct HttpMatchDto {
    /// At least one method.
    pub methods: Vec<String>,
    /// Literal or wildcard path pattern.
    pub path: String,
    /// Query parameters the proxy forwards.
    #[serde(default)]
    pub query_allowlist: Option<Vec<String>>,
    /// How a longer request path is treated.
    #[serde(default)]
    pub path_suffix_mode: Option<PathSuffixModeDto>,
}

/// `match.path_suffix_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum PathSuffixModeDto {
    /// Longer request paths are not matched.
    #[default]
    Disabled,
    /// The suffix is appended to the upstream path.
    Append,
}

impl From<model::PathSuffixMode> for PathSuffixModeDto {
    fn from(value: model::PathSuffixMode) -> Self {
        match value {
            model::PathSuffixMode::Disabled => Self::Disabled,
            model::PathSuffixMode::Append => Self::Append,
        }
    }
}

impl From<PathSuffixModeDto> for model::PathSuffixMode {
    fn from(value: PathSuffixModeDto) -> Self {
        match value {
            PathSuffixModeDto::Disabled => model::PathSuffixMode::Disabled,
            PathSuffixModeDto::Append => model::PathSuffixMode::Append,
        }
    }
}

/// gRPC matching rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatchDto {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// Method name, or `*` for the whole service.
    pub method: String,
}

/// Protocol-scoped match rules: exactly one of `http` / `grpc`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum MatchDto {
    /// HTTP matching rules.
    Http(HttpMatchDto),
    /// gRPC matching rules.
    Grpc(GrpcMatchDto),
}

impl TryFrom<&MatchDto> for model::RouteMatcher {
    type Error = DomainError;

    fn try_from(value: &MatchDto) -> Result<Self, Self::Error> {
        Ok(match value {
            MatchDto::Http(http) => model::RouteMatcher::Http(model::HttpMatch {
                methods: http.methods.clone(),
                path: http.path.clone(),
                query_allowlist: http.query_allowlist.clone().unwrap_or_default(),
                path_suffix_mode: http.path_suffix_mode.map_or(
                    model::PathSuffixMode::default(),
                    model::PathSuffixMode::from,
                ),
            }),
            MatchDto::Grpc(grpc) => model::RouteMatcher::Grpc(model::GrpcMatch {
                service: grpc.service.clone(),
                method: grpc.method.clone(),
            }),
        })
    }
}

impl From<&model::RouteMatcher> for MatchDto {
    fn from(value: &model::RouteMatcher) -> Self {
        match value {
            model::RouteMatcher::Http(http) => MatchDto::Http(HttpMatchDto {
                methods: http.methods.clone(),
                path: http.path.clone(),
                query_allowlist: Some(http.query_allowlist.clone()),
                path_suffix_mode: Some(http.path_suffix_mode.into()),
            }),
            model::RouteMatcher::Grpc(grpc) => MatchDto::Grpc(GrpcMatchDto {
                service: grpc.service.clone(),
                method: grpc.method.clone(),
            }),
        }
    }
}

/// Response body for a route.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The upstream this route forwards to.
    pub upstream_id: Uuid,
    /// Optional stable route name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Lower-case discovery tags.
    pub tags: Vec<String>,
    /// Match rule.
    #[serde(rename = "match")]
    pub matcher: MatchDto,
    /// Lower runs first.
    pub priority: i32,
    /// Disabled routes are not matched by the proxy.
    pub enabled: bool,
    /// Plugin chain, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Rate limit, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<serde_json::Value>,
    /// CORS policy, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<serde_json::Value>,
    /// Creation instant, unix epoch milliseconds.
    pub created_at: u64,
    /// Last mutation instant, unix epoch milliseconds.
    pub updated_at: u64,
}

impl RouteDto {
    /// Project a domain route onto the wire.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when a policy block cannot be serialized.
    pub fn from_entity(value: &model::Route) -> Result<Self, DomainError> {
        Ok(Self {
            id: value.id,
            tenant_id: value.tenant_id,
            upstream_id: value.upstream_id,
            name: value.name.clone(),
            tags: value.tags.clone(),
            matcher: MatchDto::from(&value.matcher),
            priority: value.priority,
            enabled: value.enabled,
            plugins: value.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: crate::api::rest::params::encode_policy(&value.rate_limit)?,
            cors: crate::api::rest::params::encode_policy(&value.cors)?,
            created_at: value.created_at,
            updated_at: value.updated_at,
        })
    }
}

/// Request body for `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    /// Immutable resource id; tolerated when it echoes the stored value.
    pub id: Option<Uuid>,
    /// Immutable owning tenant; tolerated when it echoes the caller's tenant.
    pub tenant_id: Option<Uuid>,
    /// Immutable on replace; required on create.
    pub upstream_id: Option<Uuid>,
    /// Optional route name.
    pub name: Option<String>,
    /// Cleared when omitted on a replace.
    pub tags: Option<Vec<String>>,
    /// Match rule (required).
    #[serde(rename = "match")]
    pub matcher: Option<MatchDto>,
    /// Defaults to 0 on create and replace.
    pub priority: Option<i32>,
    /// Defaults to `true`.
    pub enabled: Option<bool>,
    /// Plugin chain; cleared when omitted.
    pub plugins: Option<PluginChainDto>,
    /// Rate limit; cleared when omitted.
    pub rate_limit: Option<serde_json::Value>,
    /// CORS policy; cleared when omitted.
    pub cors: Option<serde_json::Value>,
}

impl RouteRequest {
    /// Convert the request into a domain specification.
    ///
    /// # Errors
    ///
    /// [`DomainError`] for a malformed policy block.
    pub fn into_spec(self) -> Result<RouteSpec, DomainError> {
        let matcher = match &self.matcher {
            Some(matcher) => Some(model::RouteMatcher::try_from(matcher)?),
            None => None,
        };
        Ok(RouteSpec {
            upstream_id: self.upstream_id,
            name: self.name,
            tags: self.tags,
            matcher,
            priority: self.priority,
            enabled: self.enabled,
            plugins: self.plugins.map(|chain| model::PluginsConfig {
                sharing: chain.sharing.into(),
                items: chain.items.iter().map(model::PluginBinding::from).collect(),
            }),
            rate_limit: crate::api::rest::params::decode_policy("rate_limit", &self.rate_limit)?,
            cors: crate::api::rest::params::decode_policy("cors", &self.cors)?,
        })
    }
}

/// Paginated route list.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteListDto {
    /// One page of routes.
    pub items: Vec<RouteDto>,
    /// Number of routes matching the filter, before paging.
    pub total: usize,
    /// Page size applied.
    pub top: usize,
    /// Page offset applied.
    pub skip: usize,
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Plugin family of a custom plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum PluginKindDto {
    /// Credential injection.
    #[default]
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request / response mutation.
    Transform,
}

impl From<model::PluginKind> for PluginKindDto {
    fn from(value: model::PluginKind) -> Self {
        match value {
            model::PluginKind::Auth => Self::Auth,
            model::PluginKind::Guard => Self::Guard,
            model::PluginKind::Transform => Self::Transform,
        }
    }
}

impl From<PluginKindDto> for model::PluginKind {
    fn from(value: PluginKindDto) -> Self {
        match value {
            PluginKindDto::Auth => model::PluginKind::Auth,
            PluginKindDto::Guard => model::PluginKind::Guard,
            PluginKindDto::Transform => model::PluginKind::Transform,
        }
    }
}

/// Response body for a custom plugin row (without its Starlark source, which
/// has a dedicated endpoint).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Tenant-unique plugin name.
    pub name: String,
    /// Plugin family.
    #[serde(rename = "type")]
    pub kind: PluginKindDto,
    /// Free-text description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Configuration document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Creation instant, unix epoch milliseconds.
    pub created_at: u64,
    /// Last mutation instant, unix epoch milliseconds.
    pub updated_at: u64,
}

impl From<&model::Plugin> for PluginDto {
    fn from(value: &model::Plugin) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id,
            name: value.name.clone(),
            kind: value.kind.into(),
            description: value.description.clone(),
            config: value.config.clone(),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Request body for `POST /oagw/v1/plugins`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreatePluginRequest {
    /// Tenant-unique plugin name.
    pub name: String,
    /// Plugin family.
    #[serde(rename = "type")]
    pub kind: PluginKindDto,
    /// Free-text description.
    #[serde(default)]
    pub description: Option<String>,
    /// Configuration document.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// Sandboxed Starlark source.
    pub source: String,
}

impl CreatePluginRequest {
    /// Convert the request into a domain specification.
    #[must_use]
    pub fn into_spec(self) -> PluginSpec {
        PluginSpec {
            name: Some(self.name),
            kind: Some(self.kind.into()),
            description: self.description,
            config: self.config,
            source: Some(self.source),
        }
    }
}

/// Response body for `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// The plugin's anonymous GTS id.
    pub plugin_id: String,
    /// Tenant-unique plugin name.
    pub name: String,
    /// Plugin family.
    #[serde(rename = "type")]
    pub kind: PluginKindDto,
    /// Sandboxed Starlark source.
    pub source: String,
}

/// Paginated plugin list.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginListDto {
    /// One page of plugins.
    pub items: Vec<PluginDto>,
    /// Number of plugins matching the filter, before paging.
    pub total: usize,
    /// Page size applied.
    pub top: usize,
    /// Page offset applied.
    pub skip: usize,
}

/// Response body for `DELETE`-protected plugin reference sets.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReferencedByDto {
    /// Upstreams referencing the plugin, by anonymous GTS id.
    pub upstreams: Vec<String>,
    /// Routes referencing the plugin, by anonymous GTS id.
    pub routes: Vec<String>,
}

impl ReferencedByDto {
    /// Project a domain reference set.
    #[must_use]
    pub fn from_references(value: &crate::domain::error::PluginReferences) -> Self {
        Self {
            upstreams: value.upstreams.clone(),
            routes: value.routes.clone(),
        }
    }
}

/// Body of `POST /oagw/v1/upstreams/{id}/enable` and `.../disable`.
///
/// The operations carry no body; the DTO exists so the OpenAPI document can
/// declare an empty request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request)]
pub struct EmptyRequest {}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
