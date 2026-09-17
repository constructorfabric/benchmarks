//! Wire DTOs for the OAGW management REST API.
//!
//! Field shapes, defaults, and enums mirror
//! `gears/system/oagw/docs/schemas/{upstream,route}.v1.schema.json` (the
//! management wire uses snake_case naming). The domain models use camelCase
//! serde names internally; the `From` conversions below translate both ways.

use std::collections::BTreeMap;

use serde_json::Value;
use uuid::Uuid;

use crate::domain::models::{
    AuthConfig, BurstConfig, CorsConfig, Endpoint, GrpcMatch, HeaderTransforms, HttpMatch,
    PassthroughMode, PathSuffixMode, Plugin, PluginBinding, PluginsConfig, PluginItem,
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow,
    RequestHeaderRules, ResponseHeaderRules, Route, RouteMatch, ServerConfig, SharingMode,
    SustainedRate, Upstream,
};

// ---------------------------------------------------------------------------
// Small enums
// ---------------------------------------------------------------------------

/// Hierarchical config sharing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum OagwSharingModeDto {
    #[default]
    Private,
    Inherit,
    Enforce,
}

impl From<SharingMode> for OagwSharingModeDto {
    fn from(v: SharingMode) -> Self {
        match v {
            SharingMode::Private => Self::Private,
            SharingMode::Inherit => Self::Inherit,
            SharingMode::Enforce => Self::Enforce,
        }
    }
}

impl From<OagwSharingModeDto> for SharingMode {
    fn from(v: OagwSharingModeDto) -> Self {
        match v {
            OagwSharingModeDto::Private => Self::Private,
            OagwSharingModeDto::Inherit => Self::Inherit,
            OagwSharingModeDto::Enforce => Self::Enforce,
        }
    }
}

/// Inbound header passthrough mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum PassthroughModeDto {
    #[default]
    None,
    Allowlist,
    All,
}

impl From<PassthroughMode> for PassthroughModeDto {
    fn from(v: PassthroughMode) -> Self {
        match v {
            PassthroughMode::None => Self::None,
            PassthroughMode::Allowlist => Self::Allowlist,
            PassthroughMode::All => Self::All,
        }
    }
}

impl From<PassthroughModeDto> for PassthroughMode {
    fn from(v: PassthroughModeDto) -> Self {
        match v {
            PassthroughModeDto::None => Self::None,
            PassthroughModeDto::Allowlist => Self::Allowlist,
            PassthroughModeDto::All => Self::All,
        }
    }
}

/// Path suffix behavior for `/proxy/{alias}/{*path}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum PathSuffixModeDto {
    Disabled,
    #[default]
    Append,
}

impl From<PathSuffixMode> for PathSuffixModeDto {
    fn from(v: PathSuffixMode) -> Self {
        match v {
            PathSuffixMode::Disabled => Self::Disabled,
            PathSuffixMode::Append => Self::Append,
        }
    }
}

impl From<PathSuffixModeDto> for PathSuffixMode {
    fn from(v: PathSuffixModeDto) -> Self {
        match v {
            PathSuffixModeDto::Disabled => Self::Disabled,
            PathSuffixModeDto::Append => Self::Append,
        }
    }
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateLimitAlgorithmDto {
    #[default]
    TokenBucket,
    SlidingWindow,
}

impl From<RateLimitAlgorithm> for RateLimitAlgorithmDto {
    fn from(v: RateLimitAlgorithm) -> Self {
        match v {
            RateLimitAlgorithm::TokenBucket => Self::TokenBucket,
            RateLimitAlgorithm::SlidingWindow => Self::SlidingWindow,
        }
    }
}

impl From<RateLimitAlgorithmDto> for RateLimitAlgorithm {
    fn from(v: RateLimitAlgorithmDto) -> Self {
        match v {
            RateLimitAlgorithmDto::TokenBucket => Self::TokenBucket,
            RateLimitAlgorithmDto::SlidingWindow => Self::SlidingWindow,
        }
    }
}

/// Rate limit time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateLimitWindowDto {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl From<RateLimitWindow> for RateLimitWindowDto {
    fn from(v: RateLimitWindow) -> Self {
        match v {
            RateLimitWindow::Second => Self::Second,
            RateLimitWindow::Minute => Self::Minute,
            RateLimitWindow::Hour => Self::Hour,
            RateLimitWindow::Day => Self::Day,
        }
    }
}

impl From<RateLimitWindowDto> for RateLimitWindow {
    fn from(v: RateLimitWindowDto) -> Self {
        match v {
            RateLimitWindowDto::Second => Self::Second,
            RateLimitWindowDto::Minute => Self::Minute,
            RateLimitWindowDto::Hour => Self::Hour,
            RateLimitWindowDto::Day => Self::Day,
        }
    }
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateLimitScopeDto {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

impl From<RateLimitScope> for RateLimitScopeDto {
    fn from(v: RateLimitScope) -> Self {
        match v {
            RateLimitScope::Global => Self::Global,
            RateLimitScope::Tenant => Self::Tenant,
            RateLimitScope::User => Self::User,
            RateLimitScope::Ip => Self::Ip,
            RateLimitScope::Route => Self::Route,
        }
    }
}

impl From<RateLimitScopeDto> for RateLimitScope {
    fn from(v: RateLimitScopeDto) -> Self {
        match v {
            RateLimitScopeDto::Global => Self::Global,
            RateLimitScopeDto::Tenant => Self::Tenant,
            RateLimitScopeDto::User => Self::User,
            RateLimitScopeDto::Ip => Self::Ip,
            RateLimitScopeDto::Route => Self::Route,
        }
    }
}

/// Rate limit strategy when capacity is exceeded (only `reject` is enforced).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateLimitStrategyDto {
    #[default]
    Reject,
    Queue,
    Degrade,
}

impl From<RateLimitStrategy> for RateLimitStrategyDto {
    fn from(v: RateLimitStrategy) -> Self {
        match v {
            RateLimitStrategy::Reject => Self::Reject,
            RateLimitStrategy::Queue => Self::Queue,
            RateLimitStrategy::Degrade => Self::Degrade,
        }
    }
}

impl From<RateLimitStrategyDto> for RateLimitStrategy {
    fn from(v: RateLimitStrategyDto) -> Self {
        match v {
            RateLimitStrategyDto::Reject => Self::Reject,
            RateLimitStrategyDto::Queue => Self::Queue,
            RateLimitStrategyDto::Degrade => Self::Degrade,
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream tree
// ---------------------------------------------------------------------------

/// A single upstream endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct EndpointDto {
    /// `https` (default), `wss`, `wt`, `grpc`; `http` only when the gear is
    /// configured with `allow_http_upstream`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname or IP address.
    pub host: String,
    /// Service port (default 443).
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_scheme() -> String {
    "https".to_owned()
}

fn default_port() -> u16 {
    443
}

impl From<Endpoint> for EndpointDto {
    fn from(e: Endpoint) -> Self {
        Self {
            scheme: e.scheme,
            host: e.host,
            port: e.port,
        }
    }
}

impl From<EndpointDto> for Endpoint {
    fn from(e: EndpointDto) -> Self {
        Self {
            scheme: e.scheme,
            host: e.host,
            port: e.port,
        }
    }
}

/// Server configuration for an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct ServerDto {
    /// One or more endpoints.
    pub endpoints: Vec<EndpointDto>,
}

impl From<ServerConfig> for ServerDto {
    fn from(s: ServerConfig) -> Self {
        Self {
            endpoints: s.endpoints.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ServerDto> for ServerConfig {
    fn from(s: ServerDto) -> Self {
        Self {
            endpoints: s.endpoints.into_iter().map(Into::into).collect(),
        }
    }
}

/// Authentication configuration for an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[derive(Default)]
pub struct AuthDto {
    /// Auth plugin type (GTS identifier of an `auth_plugin`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Sharing mode for hierarchical config.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Auth plugin configuration (free-form).
    #[serde(default, skip_serializing_if = "is_empty_object")]
    pub config: Value,
}

fn is_empty_object(v: &Value) -> bool {
    v.is_null() || (v.as_object().is_some_and(|o| o.is_empty()))
}

impl From<AuthConfig> for AuthDto {
    fn from(a: AuthConfig) -> Self {
        Self {
            plugin_type: a.plugin_type,
            sharing: a.sharing.into(),
            config: a.config,
        }
    }
}

impl From<AuthDto> for AuthConfig {
    fn from(a: AuthDto) -> Self {
        Self {
            plugin_type: a.plugin_type,
            sharing: a.sharing.into(),
            config: a.config,
        }
    }
}

/// Inbound request header rules.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[derive(Default)]
pub struct RequestHeaderRulesDto {
    /// Headers to set (overwrite) on the outbound request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, allow duplicates).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: PassthroughModeDto,
    /// Headers forwarded when `passthrough == allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

impl From<RequestHeaderRules> for RequestHeaderRulesDto {
    fn from(r: RequestHeaderRules) -> Self {
        Self {
            set: r.set,
            add: r.add,
            remove: r.remove,
            passthrough: r.passthrough.into(),
            passthrough_allowlist: r.passthrough_allowlist,
        }
    }
}

impl From<RequestHeaderRulesDto> for RequestHeaderRules {
    fn from(r: RequestHeaderRulesDto) -> Self {
        Self {
            set: r.set,
            add: r.add,
            remove: r.remove,
            passthrough: r.passthrough.into(),
            passthrough_allowlist: r.passthrough_allowlist,
        }
    }
}

/// Upstream response header rules.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[derive(Default)]
pub struct ResponseHeaderRulesDto {
    /// Headers to set on the client-facing response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add on the client-facing response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

impl From<ResponseHeaderRules> for ResponseHeaderRulesDto {
    fn from(r: ResponseHeaderRules) -> Self {
        Self {
            set: r.set,
            add: r.add,
            remove: r.remove,
        }
    }
}

impl From<ResponseHeaderRulesDto> for ResponseHeaderRules {
    fn from(r: ResponseHeaderRulesDto) -> Self {
        Self {
            set: r.set,
            add: r.add,
            remove: r.remove,
        }
    }
}

/// Header transformation rules for an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[derive(Default)]
pub struct HeaderTransformsDto {
    #[serde(default)]
    pub request: RequestHeaderRulesDto,
    #[serde(default)]
    pub response: ResponseHeaderRulesDto,
}

impl From<HeaderTransforms> for HeaderTransformsDto {
    fn from(h: HeaderTransforms) -> Self {
        Self {
            request: h.request.into(),
            response: h.response.into(),
        }
    }
}

impl From<HeaderTransformsDto> for HeaderTransforms {
    fn from(h: HeaderTransformsDto) -> Self {
        Self {
            request: h.request.into(),
            response: h.response.into(),
        }
    }
}

/// Explicit plugin binding with instance-level config.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct PluginBindingDto {
    pub plugin_ref: String,
    #[serde(default)]
    pub config: Value,
}

impl From<PluginBinding> for PluginBindingDto {
    fn from(b: PluginBinding) -> Self {
        Self {
            plugin_ref: b.plugin_ref,
            config: b.config,
        }
    }
}

impl From<PluginBindingDto> for PluginBinding {
    fn from(b: PluginBindingDto) -> Self {
        Self {
            plugin_ref: b.plugin_ref,
            config: b.config,
        }
    }
}

/// A single plugin reference within `plugins.items`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[serde(untagged)]
pub enum PluginItemDto {
    Ref(String),
    Binding(PluginBindingDto),
}

impl From<PluginItem> for PluginItemDto {
    fn from(i: PluginItem) -> Self {
        match i {
            PluginItem::Ref(id) => Self::Ref(id),
            PluginItem::Binding(b) => Self::Binding(b.into()),
        }
    }
}

impl From<PluginItemDto> for PluginItem {
    fn from(i: PluginItemDto) -> Self {
        match i {
            PluginItemDto::Ref(id) => Self::Ref(id),
            PluginItemDto::Binding(b) => Self::Binding(b.into()),
        }
    }
}

/// Plugin bindings for an upstream or route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[derive(Default)]
pub struct PluginsConfigDto {
    /// Sharing mode for the plugin chain.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Ordered plugin list.
    #[serde(default)]
    pub items: Vec<PluginItemDto>,
}

impl From<PluginsConfig> for PluginsConfigDto {
    fn from(p: PluginsConfig) -> Self {
        Self {
            sharing: p.sharing.into(),
            items: p.items.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PluginsConfigDto> for PluginsConfig {
    fn from(p: PluginsConfigDto) -> Self {
        Self {
            sharing: p.sharing.into(),
            items: p.items.into_iter().map(Into::into).collect(),
        }
    }
}

/// Sustained refill policy.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct SustainedRateDto {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Time window.
    #[serde(default)]
    pub window: RateLimitWindowDto,
}

impl From<SustainedRate> for SustainedRateDto {
    fn from(s: SustainedRate) -> Self {
        Self {
            rate: s.rate,
            window: s.window.into(),
        }
    }
}

impl From<SustainedRateDto> for SustainedRate {
    fn from(s: SustainedRateDto) -> Self {
        Self {
            rate: s.rate,
            window: s.window.into(),
        }
    }
}

/// Burst configuration.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct BurstDto {
    /// Maximum burst size (bucket capacity).
    #[serde(default = "default_one")]
    pub capacity: u32,
}

fn default_one() -> u32 {
    1
}

impl From<BurstConfig> for BurstDto {
    fn from(b: BurstConfig) -> Self {
        Self { capacity: b.capacity }
    }
}

impl From<BurstDto> for BurstConfig {
    fn from(b: BurstDto) -> Self {
        Self { capacity: b.capacity }
    }
}

/// Rate limiting configuration (upstream or route scoped).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct RateLimitDto {
    /// Sharing mode for hierarchical composition.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Algorithm (only `token_bucket` is implemented).
    #[serde(default)]
    pub algorithm: RateLimitAlgorithmDto,
    /// Sustained refill policy.
    pub sustained: SustainedRateDto,
    /// Burst capacity (defaults to `sustained.rate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstDto>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateLimitScopeDto,
    /// Excess behavior (`reject` implemented; others degrade to reject).
    #[serde(default)]
    pub strategy: RateLimitStrategyDto,
    /// Tokens consumed per request.
    #[serde(default = "default_one")]
    pub cost: u32,
}

impl From<RateLimitConfig> for RateLimitDto {
    fn from(r: RateLimitConfig) -> Self {
        Self {
            sharing: r.sharing.into(),
            algorithm: r.algorithm.into(),
            sustained: r.sustained.into(),
            burst: r.burst.map(Into::into),
            scope: r.scope.into(),
            strategy: r.strategy.into(),
            cost: r.cost,
        }
    }
}

impl From<RateLimitDto> for RateLimitConfig {
    fn from(r: RateLimitDto) -> Self {
        Self {
            sharing: r.sharing.into(),
            algorithm: r.algorithm.into(),
            sustained: r.sustained.into(),
            burst: r.burst.map(Into::into),
            scope: r.scope.into(),
            strategy: r.strategy.into(),
            cost: r.cost,
        }
    }
}

/// CORS configuration for an upstream or route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct CorsDto {
    /// Sharing mode for hierarchical composition.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Enable CORS enforcement.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins (`["*"]` for any; not allowed with credentials).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods (default `["GET", "POST"]`).
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Allow credentials (requires specific origins, not `*`).
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl From<CorsConfig> for CorsDto {
    fn from(c: CorsConfig) -> Self {
        Self {
            sharing: c.sharing.into(),
            enabled: c.enabled,
            allowed_origins: c.allowed_origins,
            allowed_methods: c.allowed_methods,
            expose_headers: c.expose_headers,
            allow_credentials: c.allow_credentials,
        }
    }
}

impl From<CorsDto> for CorsConfig {
    fn from(c: CorsDto) -> Self {
        Self {
            sharing: c.sharing.into(),
            enabled: c.enabled,
            allowed_origins: c.allowed_origins,
            allowed_methods: c.allowed_methods,
            expose_headers: c.expose_headers,
            allow_credentials: c.allow_credentials,
        }
    }
}

/// An upstream resource.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct UpstreamDto {
    /// System-generated identifier (server-managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant (server-managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Whether requests to this upstream are allowed.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Human-readable routing identifier (also the `/proxy/{alias}` key).
    pub alias: String,
    /// Flat tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint set.
    pub server: ServerDto,
    /// Upstream protocol as GTS identifier.
    pub protocol: String,
    /// Upstream authentication.
    #[serde(default)]
    pub auth: AuthDto,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeaderTransformsDto,
    /// Plugin bindings.
    #[serde(default)]
    pub plugins: PluginsConfigDto,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

fn default_true() -> bool {
    true
}

impl From<Upstream> for UpstreamDto {
    fn from(u: Upstream) -> Self {
        Self {
            id: u.id,
            tenant_id: u.tenant_id,
            enabled: u.enabled,
            alias: u.alias,
            tags: u.tags,
            server: u.server.into(),
            protocol: u.protocol,
            auth: u.auth.into(),
            headers: u.headers.into(),
            plugins: u.plugins.into(),
            rate_limit: u.rate_limit.map(Into::into),
            cors: u.cors.map(Into::into),
        }
    }
}

impl From<UpstreamDto> for Upstream {
    fn from(u: UpstreamDto) -> Self {
        Self {
            id: u.id,
            tenant_id: u.tenant_id,
            enabled: u.enabled,
            alias: u.alias,
            tags: u.tags,
            server: u.server.into(),
            protocol: u.protocol,
            auth: u.auth.into(),
            headers: u.headers.into(),
            plugins: u.plugins.into(),
            rate_limit: u.rate_limit.map(Into::into),
            cors: u.cors.map(Into::into),
        }
    }
}

// ---------------------------------------------------------------------------
// Route tree
// ---------------------------------------------------------------------------

/// HTTP match rules.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct RouteHttpMatchDto {
    /// Allowed methods, min 1.
    pub methods: Vec<String>,
    /// Path prefix, min length 1.
    pub path: String,
    /// Whitelisted query parameters (empty = allow none).
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Path suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixModeDto,
}

impl From<HttpMatch> for RouteHttpMatchDto {
    fn from(m: HttpMatch) -> Self {
        Self {
            methods: m.methods,
            path: m.path,
            query_allowlist: m.query_allowlist,
            path_suffix_mode: m.path_suffix_mode.into(),
        }
    }
}

impl From<RouteHttpMatchDto> for HttpMatch {
    fn from(m: RouteHttpMatchDto) -> Self {
        Self {
            methods: m.methods,
            path: m.path,
            query_allowlist: m.query_allowlist,
            path_suffix_mode: m.path_suffix_mode.into(),
        }
    }
}

/// gRPC match rules.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct RouteGrpcMatchDto {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

impl From<GrpcMatch> for RouteGrpcMatchDto {
    fn from(m: GrpcMatch) -> Self {
        Self {
            service: m.service,
            method: m.method,
        }
    }
}

impl From<RouteGrpcMatchDto> for GrpcMatch {
    fn from(m: RouteGrpcMatchDto) -> Self {
        Self {
            service: m.service,
            method: m.method,
        }
    }
}

/// Route match: either HTTP rules or gRPC rules, exactly one.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
#[serde(untagged)]
pub enum RouteMatchDto {
    Http(RouteHttpMatchDto),
    Grpc(RouteGrpcMatchDto),
}

impl From<RouteMatch> for RouteMatchDto {
    fn from(m: RouteMatch) -> Self {
        match m {
            RouteMatch::Http(h) => Self::Http(h.into()),
            RouteMatch::Grpc(g) => Self::Grpc(g.into()),
        }
    }
}

impl From<RouteMatchDto> for RouteMatch {
    fn from(m: RouteMatchDto) -> Self {
        match m {
            RouteMatchDto::Http(h) => Self::Http(h.into()),
            RouteMatchDto::Grpc(g) => Self::Grpc(g.into()),
        }
    }
}

/// A route binding a match rule to an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct RouteDto {
    /// System-generated identifier (server-managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant (server-managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Target upstream (belongs to the same tenant).
    pub upstream_id: Uuid,
    /// Route is active.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match rules (exactly one of `http` | `grpc`).
    #[serde(rename = "match")]
    pub match_: RouteMatchDto,
    /// Flat tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Route-level plugin bindings.
    #[serde(default)]
    pub plugins: PluginsConfigDto,
    /// Route-level rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    /// Route-level CORS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

impl From<Route> for RouteDto {
    fn from(r: Route) -> Self {
        Self {
            id: r.id,
            tenant_id: r.tenant_id,
            upstream_id: r.upstream_id,
            enabled: r.enabled,
            match_: r.match_.into(),
            tags: r.tags,
            plugins: r.plugins.into(),
            rate_limit: r.rate_limit.map(Into::into),
            cors: r.cors.map(Into::into),
        }
    }
}

impl From<RouteDto> for Route {
    fn from(r: RouteDto) -> Self {
        Self {
            id: r.id,
            tenant_id: r.tenant_id,
            upstream_id: r.upstream_id,
            enabled: r.enabled,
            match_: r.match_.into(),
            tags: r.tags,
            plugins: r.plugins.into(),
            rate_limit: r.rate_limit.map(Into::into),
            cors: r.cors.map(Into::into),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin tree
// ---------------------------------------------------------------------------

/// A custom tenant-defined plugin definition.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response, request)]
pub struct PluginDto {
    /// System-generated identifier (server-managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant (server-managed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Plugin type (GTS identifier of the plugin category being customized).
    pub plugin_type: String,
    /// Display name.
    pub name: String,
    /// JSON schema describing the plugin config surface.
    #[serde(default, skip_serializing_if = "is_empty_object")]
    pub config_schema: Value,
    /// Starlark source code.
    pub source_code: String,
}

impl From<Plugin> for PluginDto {
    fn from(p: Plugin) -> Self {
        Self {
            id: p.id,
            tenant_id: p.tenant_id,
            plugin_type: p.plugin_type,
            name: p.name,
            config_schema: p.config_schema,
            source_code: p.source_code,
        }
    }
}

impl From<PluginDto> for Plugin {
    fn from(p: PluginDto) -> Self {
        Self {
            id: p.id,
            tenant_id: p.tenant_id,
            plugin_type: p.plugin_type,
            name: p.name,
            config_schema: p.config_schema,
            source_code: p.source_code,
        }
    }
}
