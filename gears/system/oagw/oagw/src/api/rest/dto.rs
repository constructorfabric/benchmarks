//! REST DTOs for the OAGW management API.
//!
//! These mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` (same field names, same defaults) and
//! convert losslessly to and from the domain models in
//! [`crate::domain::model`].
//!
//! `id` is server-generated and read-only; `tenant_id` is never exposed.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::plugin::{Plugin, PluginKind, PluginSource, PluginSourceRecord};
use crate::domain::model::route::{GrpcMatch, HttpMatch, Route, RouteMatch};
use crate::domain::model::upstream::{
    AuthBinding, Endpoint, Protocol, Scheme, ServerConfig, Upstream,
};
use crate::domain::model::{
    BurstCapacity, CorsConfig, HeaderPassthrough, HeaderRules, PluginBinding, PluginChain,
    PluginRef, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    RequestHeaderRules, ResponseHeaderRules, SharingMode, SustainedRate,
};
use crate::domain::services::{PluginDraft, RouteDraft, UpstreamDraft};

/// Boolean default used by request DTOs (`true` = enabled).
#[must_use]
pub fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Shared vocabulary
// ---------------------------------------------------------------------------

/// Sharing mode on the wire (`private|inherit|enforce`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum OagwSharingModeDto {
    /// Not visible to descendant tenants.
    #[default]
    Private,
    /// Visible to descendants, which may override it.
    Inherit,
    /// Visible to descendants, which may **not** override it.
    Enforce,
}

impl From<SharingMode> for OagwSharingModeDto {
    fn from(value: SharingMode) -> Self {
        match value {
            SharingMode::Private => Self::Private,
            SharingMode::Inherit => Self::Inherit,
            SharingMode::Enforce => Self::Enforce,
        }
    }
}

impl From<OagwSharingModeDto> for SharingMode {
    fn from(value: OagwSharingModeDto) -> Self {
        match value {
            OagwSharingModeDto::Private => SharingMode::Private,
            OagwSharingModeDto::Inherit => SharingMode::Inherit,
            OagwSharingModeDto::Enforce => SharingMode::Enforce,
        }
    }
}

/// Endpoint scheme on the wire.
///
/// `http` is accepted **unconditionally**: `allow_http_upstream` governs only
/// whether a plaintext connection is dialled at proxy time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(response, request)]
pub enum SchemeDto {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebSocket (cleartext).
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl From<Scheme> for SchemeDto {
    fn from(value: Scheme) -> Self {
        match value {
            Scheme::Http => Self::Http,
            Scheme::Https => Self::Https,
            Scheme::Wss => Self::Wss,
            Scheme::Wt => Self::Wt,
            Scheme::Grpc => Self::Grpc,
        }
    }
}

impl From<SchemeDto> for Scheme {
    fn from(value: SchemeDto) -> Self {
        match value {
            SchemeDto::Http => Scheme::Http,
            SchemeDto::Https => Scheme::Https,
            SchemeDto::Wss => Scheme::Wss,
            SchemeDto::Wt => Scheme::Wt,
            SchemeDto::Grpc => Scheme::Grpc,
        }
    }
}

/// Endpoint on the wire (`server.endpoints[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct EndpointDto {
    /// Endpoint scheme; defaults to `https`.
    #[serde(default)]
    pub scheme: SchemeDto,
    /// Hostname or IP address.
    #[serde(default)]
    pub host: String,
    /// Port; defaults to the scheme's standard port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Default for EndpointDto {
    fn default() -> Self {
        Self {
            scheme: SchemeDto::Https,
            host: String::new(),
            port: None,
        }
    }
}

impl From<&Endpoint> for EndpointDto {
    fn from(value: &Endpoint) -> Self {
        Self {
            scheme: value.scheme.into(),
            host: value.host.clone(),
            port: Some(value.effective_port()),
        }
    }
}

impl TryFrom<&EndpointDto> for Endpoint {
    type Error = DomainError;

    fn try_from(value: &EndpointDto) -> Result<Self, Self::Error> {
        Ok(Self {
            scheme: value.scheme.into(),
            host: value.host.clone(),
            port: value.port,
        })
    }
}

/// `server` block on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct ServerConfigDto {
    /// Endpoints; at least one.
    #[serde(default)]
    pub endpoints: Vec<EndpointDto>,
}

impl From<&ServerConfig> for ServerConfigDto {
    fn from(value: &ServerConfig) -> Self {
        Self {
            endpoints: value.endpoints.iter().map(EndpointDto::from).collect(),
        }
    }
}

impl TryFrom<&ServerConfigDto> for ServerConfig {
    type Error = DomainError;

    fn try_from(value: &ServerConfigDto) -> Result<Self, Self::Error> {
        let mut endpoints = Vec::with_capacity(value.endpoints.len());
        for endpoint in &value.endpoints {
            endpoints.push(Endpoint::try_from(endpoint)?);
        }
        Ok(Self { endpoints })
    }
}

/// Upstream protocol on the wire (full GTS instance ids).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(response, request)]
pub enum ProtocolDto {
    /// HTTP/1.1 and HTTP/2.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    #[default]
    Http,
    /// gRPC.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl From<Protocol> for ProtocolDto {
    fn from(value: Protocol) -> Self {
        match value {
            Protocol::Http => Self::Http,
            Protocol::Grpc => Self::Grpc,
        }
    }
}

impl From<ProtocolDto> for Protocol {
    fn from(value: ProtocolDto) -> Self {
        match value {
            ProtocolDto::Http => Protocol::Http,
            ProtocolDto::Grpc => Protocol::Grpc,
        }
    }
}

/// Plugin reference on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct PluginRefDto {
    /// The canonical plugin identifier (full GTS identifier or UUID).
    #[serde(default)]
    pub plugin_ref: String,
    /// Extracted UUID when the reference is UUID-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
}

impl From<&PluginRef> for PluginRefDto {
    fn from(value: &PluginRef) -> Self {
        Self {
            plugin_ref: value.plugin_ref.clone(),
            plugin_uuid: value.plugin_uuid,
        }
    }
}

impl From<&PluginRefDto> for PluginRef {
    fn from(value: &PluginRefDto) -> Self {
        Self {
            plugin_ref: value.plugin_ref.clone(),
            plugin_uuid: value.plugin_uuid,
        }
    }
}

/// An entry of `plugins.items[]`.
///
/// Accepts both wire forms the schemas and ADR 0009 show: a bare identifier
/// (`"gts.cf.core.oagw.guard_plugin.v1~…"` or a plugin UUID) and the object
/// form (`{"plugin_ref": …, "config": {…}}`). Serialisation always emits the
/// object form.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
#[serde(untagged)]
pub enum PluginBindingDto {
    /// Bare plugin reference with no per-binding configuration.
    Bare(String),
    /// Plugin reference plus per-binding configuration.
    Detailed {
        /// Which plugin is bound.
        plugin_ref: String,
        /// Extracted UUID when the reference is UUID-backed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plugin_uuid: Option<Uuid>,
        /// Per-binding plugin configuration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Value>,
    },
}

impl From<&PluginBinding> for PluginBindingDto {
    fn from(value: &PluginBinding) -> Self {
        let plugin_ref = value.plugin_ref().to_owned();
        let plugin_uuid = crate::domain::services::alias::plugin_uuid_from_ref(&plugin_ref);
        Self::Detailed {
            plugin_ref,
            plugin_uuid,
            config: value.config().cloned(),
        }
    }
}

impl From<&PluginBindingDto> for PluginBinding {
    fn from(value: &PluginBindingDto) -> Self {
        match value {
            PluginBindingDto::Bare(plugin_ref) => Self::Bare(plugin_ref.clone()),
            PluginBindingDto::Detailed {
                plugin_ref, config, ..
            } => Self::Detailed {
                plugin_ref: plugin_ref.clone(),
                config: config.clone(),
            },
        }
    }
}

/// Plugin chain on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct PluginChainDto {
    /// How the chain participates in tenant-hierarchy merging.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Plugins in execution order.
    #[serde(default)]
    pub items: Vec<PluginBindingDto>,
}

impl From<&PluginChain> for PluginChainDto {
    fn from(value: &PluginChain) -> Self {
        Self {
            sharing: value.sharing.into(),
            items: value.items.iter().map(PluginBindingDto::from).collect(),
        }
    }
}

impl From<&PluginChainDto> for PluginChain {
    fn from(value: &PluginChainDto) -> Self {
        Self {
            sharing: value.sharing.into(),
            items: value.items.iter().map(PluginBinding::from).collect(),
        }
    }
}

/// Time window of a sustained rate on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateWindowDto {
    /// One second.
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

impl From<RateWindow> for RateWindowDto {
    fn from(value: RateWindow) -> Self {
        match value {
            RateWindow::Second => Self::Second,
            RateWindow::Minute => Self::Minute,
            RateWindow::Hour => Self::Hour,
            RateWindow::Day => Self::Day,
        }
    }
}

impl From<RateWindowDto> for RateWindow {
    fn from(value: RateWindowDto) -> Self {
        match value {
            RateWindowDto::Second => Self::Second,
            RateWindowDto::Minute => Self::Minute,
            RateWindowDto::Hour => Self::Hour,
            RateWindowDto::Day => Self::Day,
        }
    }
}

/// Sustained rate on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct SustainedRateDto {
    /// Tokens replenished per `window`.
    #[serde(default)]
    pub rate: u64,
    /// Window over which `rate` replenishes.
    #[serde(default)]
    pub window: RateWindowDto,
}

impl Default for SustainedRateDto {
    fn default() -> Self {
        Self {
            rate: 1,
            window: RateWindowDto::Second,
        }
    }
}

impl From<&SustainedRate> for SustainedRateDto {
    fn from(value: &SustainedRate) -> Self {
        Self {
            rate: value.rate,
            window: value.window.into(),
        }
    }
}

impl From<&SustainedRateDto> for SustainedRate {
    fn from(value: &SustainedRateDto) -> Self {
        Self {
            rate: value.rate,
            window: value.window.into(),
        }
    }
}

/// Burst capacity on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct BurstCapacityDto {
    /// Maximum burst size.
    #[serde(default)]
    pub capacity: u64,
}

impl From<&BurstCapacity> for BurstCapacityDto {
    fn from(value: &BurstCapacity) -> Self {
        Self {
            capacity: value.capacity,
        }
    }
}

impl From<&BurstCapacityDto> for BurstCapacity {
    fn from(value: &BurstCapacityDto) -> Self {
        Self {
            capacity: value.capacity,
        }
    }
}

/// Rate-limiting algorithm on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateAlgorithmDto {
    /// Token bucket.
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

impl From<RateAlgorithm> for RateAlgorithmDto {
    fn from(value: RateAlgorithm) -> Self {
        match value {
            RateAlgorithm::TokenBucket => Self::TokenBucket,
            RateAlgorithm::SlidingWindow => Self::SlidingWindow,
        }
    }
}

impl From<RateAlgorithmDto> for RateAlgorithm {
    fn from(value: RateAlgorithmDto) -> Self {
        match value {
            RateAlgorithmDto::TokenBucket => RateAlgorithm::TokenBucket,
            RateAlgorithmDto::SlidingWindow => RateAlgorithm::SlidingWindow,
        }
    }
}

/// Rate-limit scope on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateScopeDto {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

impl From<RateScope> for RateScopeDto {
    fn from(value: RateScope) -> Self {
        match value {
            RateScope::Global => Self::Global,
            RateScope::Tenant => Self::Tenant,
            RateScope::User => Self::User,
            RateScope::Ip => Self::Ip,
            RateScope::Route => Self::Route,
        }
    }
}

impl From<RateScopeDto> for RateScope {
    fn from(value: RateScopeDto) -> Self {
        match value {
            RateScopeDto::Global => RateScope::Global,
            RateScopeDto::Tenant => RateScope::Tenant,
            RateScopeDto::User => RateScope::User,
            RateScopeDto::Ip => RateScope::Ip,
            RateScopeDto::Route => RateScope::Route,
        }
    }
}

/// Rate-limit strategy on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum RateStrategyDto {
    /// Reject with 429.
    #[default]
    Reject,
    /// Queue until a token is available.
    Queue,
    /// Degrade.
    Degrade,
}

impl From<RateStrategy> for RateStrategyDto {
    fn from(value: RateStrategy) -> Self {
        match value {
            RateStrategy::Reject => Self::Reject,
            RateStrategy::Queue => Self::Queue,
            RateStrategy::Degrade => Self::Degrade,
        }
    }
}

impl From<RateStrategyDto> for RateStrategy {
    fn from(value: RateStrategyDto) -> Self {
        match value {
            RateStrategyDto::Reject => RateStrategy::Reject,
            RateStrategyDto::Queue => RateStrategy::Queue,
            RateStrategyDto::Degrade => RateStrategy::Degrade,
        }
    }
}

/// Rate-limit configuration on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct RateLimitConfigDto {
    /// How the block participates in tenant-hierarchy merging.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Rate-limiting algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithmDto,
    /// Sustained rate.
    #[serde(default)]
    pub sustained: SustainedRateDto,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacityDto>,
    /// Counter keying scope.
    #[serde(default)]
    pub scope: RateScopeDto,
    /// Behaviour when exhausted.
    #[serde(default)]
    pub strategy: RateStrategyDto,
    /// Tokens consumed per request.
    #[serde(default)]
    pub cost: u64,
}

impl Default for RateLimitConfigDto {
    fn default() -> Self {
        Self {
            sharing: OagwSharingModeDto::Private,
            algorithm: RateAlgorithmDto::TokenBucket,
            sustained: SustainedRateDto::default(),
            burst: None,
            scope: RateScopeDto::Tenant,
            strategy: RateStrategyDto::Reject,
            cost: 1,
        }
    }
}

impl From<&RateLimitConfig> for RateLimitConfigDto {
    fn from(value: &RateLimitConfig) -> Self {
        Self {
            sharing: value.sharing.into(),
            algorithm: value.algorithm.into(),
            sustained: SustainedRateDto::from(&value.sustained),
            burst: value.burst.as_ref().map(BurstCapacityDto::from),
            scope: value.scope.into(),
            strategy: value.strategy.into(),
            cost: value.cost,
        }
    }
}

impl From<&RateLimitConfigDto> for RateLimitConfig {
    fn from(value: &RateLimitConfigDto) -> Self {
        Self {
            sharing: value.sharing.into(),
            algorithm: value.algorithm.into(),
            sustained: SustainedRate::from(&value.sustained),
            burst: value.burst.as_ref().map(BurstCapacity::from),
            scope: value.scope.into(),
            strategy: value.strategy.into(),
            cost: value.cost,
        }
    }
}

/// Header passthrough on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum HeaderPassthroughDto {
    /// Forward nothing but the set the data plane adds.
    #[default]
    None,
    /// Forward only `passthrough_allowlist` names.
    Allowlist,
    /// Forward everything.
    All,
}

impl From<HeaderPassthrough> for HeaderPassthroughDto {
    fn from(value: HeaderPassthrough) -> Self {
        match value {
            HeaderPassthrough::None => Self::None,
            HeaderPassthrough::Allowlist => Self::Allowlist,
            HeaderPassthrough::All => Self::All,
        }
    }
}

impl From<HeaderPassthroughDto> for HeaderPassthrough {
    fn from(value: HeaderPassthroughDto) -> Self {
        match value {
            HeaderPassthroughDto::None => HeaderPassthrough::None,
            HeaderPassthroughDto::Allowlist => HeaderPassthrough::Allowlist,
            HeaderPassthroughDto::All => HeaderPassthrough::All,
        }
    }
}

/// Request-side header rules on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct RequestHeaderRulesDto {
    /// Headers to set (overwrite when present).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append; duplicates allowed).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: HeaderPassthroughDto,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

impl RequestHeaderRulesDto {
    /// True when nothing is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.passthrough == HeaderPassthroughDto::None
            && self.passthrough_allowlist.is_empty()
    }
}

impl From<&RequestHeaderRules> for RequestHeaderRulesDto {
    fn from(value: &RequestHeaderRules) -> Self {
        Self {
            set: value.set.clone(),
            add: value.add.clone(),
            remove: value.remove.clone(),
            passthrough: value.passthrough.into(),
            passthrough_allowlist: value.passthrough_allowlist.clone(),
        }
    }
}

impl From<&RequestHeaderRulesDto> for RequestHeaderRules {
    fn from(value: &RequestHeaderRulesDto) -> Self {
        Self {
            set: value.set.clone(),
            add: value.add.clone(),
            remove: value.remove.clone(),
            passthrough: value.passthrough.into(),
            passthrough_allowlist: value.passthrough_allowlist.clone(),
        }
    }
}

/// Response-side header rules on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct ResponseHeaderRulesDto {
    /// Headers to set on the response.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

impl ResponseHeaderRulesDto {
    /// True when nothing is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.add.is_empty() && self.remove.is_empty()
    }
}

impl From<&ResponseHeaderRules> for ResponseHeaderRulesDto {
    fn from(value: &ResponseHeaderRules) -> Self {
        Self {
            set: value.set.clone(),
            add: value.add.clone(),
            remove: value.remove.clone(),
        }
    }
}

impl From<&ResponseHeaderRulesDto> for ResponseHeaderRules {
    fn from(value: &ResponseHeaderRulesDto) -> Self {
        Self {
            set: value.set.clone(),
            add: value.add.clone(),
            remove: value.remove.clone(),
        }
    }
}

/// Header rules on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct HeaderRulesDto {
    /// Rules applied to the request sent upstream.
    #[serde(default, skip_serializing_if = "RequestHeaderRulesDto::is_empty")]
    pub request: RequestHeaderRulesDto,
    /// Rules applied to the response returned downstream.
    #[serde(default, skip_serializing_if = "ResponseHeaderRulesDto::is_empty")]
    pub response: ResponseHeaderRulesDto,
}

impl From<&HeaderRules> for HeaderRulesDto {
    fn from(value: &HeaderRules) -> Self {
        Self {
            request: RequestHeaderRulesDto::from(&value.request),
            response: ResponseHeaderRulesDto::from(&value.response),
        }
    }
}

impl From<&HeaderRulesDto> for HeaderRules {
    fn from(value: &HeaderRulesDto) -> Self {
        Self {
            request: RequestHeaderRules::from(&value.request),
            response: ResponseHeaderRules::from(&value.response),
        }
    }
}

/// CORS configuration on the wire (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct CorsConfigDto {
    /// How the block participates in tenant-hierarchy merging.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Whether CORS is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins (`["*"]` for any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

impl Default for CorsConfigDto {
    fn default() -> Self {
        Self {
            sharing: OagwSharingModeDto::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: crate::domain::model::DEFAULT_CORS_METHODS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl From<&CorsConfig> for CorsConfigDto {
    fn from(value: &CorsConfig) -> Self {
        Self {
            sharing: value.sharing.into(),
            enabled: value.enabled,
            allowed_origins: value.allowed_origins.clone(),
            allowed_methods: value.allowed_methods.clone(),
            expose_headers: value.expose_headers.clone(),
            allow_credentials: value.allow_credentials,
        }
    }
}

impl From<&CorsConfigDto> for CorsConfig {
    fn from(value: &CorsConfigDto) -> Self {
        Self {
            sharing: value.sharing.into(),
            enabled: value.enabled,
            allowed_origins: value.allowed_origins.clone(),
            allowed_methods: value.allowed_methods.clone(),
            expose_headers: value.expose_headers.clone(),
            allow_credentials: value.allow_credentials,
        }
    }
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// `auth` block on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct AuthBindingDto {
    /// Auth plugin identifier (GTS instance id).
    #[serde(default, rename = "type", alias = "plugin_ref")]
    pub plugin_type: String,
    /// How the binding participates in tenant-hierarchy merging.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

impl From<&AuthBinding> for AuthBindingDto {
    fn from(value: &AuthBinding) -> Self {
        Self {
            plugin_type: value.plugin.plugin_ref.clone(),
            sharing: value.sharing.into(),
            config: value.config.clone(),
        }
    }
}

impl From<&AuthBindingDto> for AuthBinding {
    fn from(value: &AuthBindingDto) -> Self {
        Self {
            plugin: PluginRef::named(value.plugin_type.clone()),
            sharing: value.sharing.into(),
            config: value.config.clone(),
        }
    }
}

/// Request body for `POST /oagw/v1/upstreams` and
/// `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct UpstreamRequestDto {
    /// Whether the upstream accepts traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing identifier; omitted means "derive from the endpoints".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoints.
    #[serde(default)]
    pub server: ServerConfigDto,
    /// Upstream protocol.
    #[serde(default)]
    pub protocol: ProtocolDto,
    /// Authentication binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthBindingDto>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderRulesDto>,
    /// Upstream-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Upstream-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfigDto>,
}

impl From<&Upstream> for UpstreamRequestDto {
    fn from(value: &Upstream) -> Self {
        Self {
            enabled: value.enabled,
            alias: Some(value.alias.clone()),
            tags: value.tags.clone(),
            server: ServerConfigDto::from(&value.server),
            protocol: value.protocol.into(),
            auth: value.auth.as_ref().map(AuthBindingDto::from),
            headers: value.headers.as_ref().map(HeaderRulesDto::from),
            plugins: value.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: value.rate_limit.as_ref().map(RateLimitConfigDto::from),
            cors: value.cors.as_ref().map(CorsConfigDto::from),
        }
    }
}

/// Response body for the upstream endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamResponseDto {
    /// Server-generated id.
    pub id: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Routing identifier.
    pub alias: String,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoints.
    pub server: ServerConfigDto,
    /// Upstream protocol (GTS instance id).
    pub protocol: ProtocolDto,
    /// Authentication binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthBindingDto>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderRulesDto>,
    /// Upstream-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Upstream-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfigDto>,
    /// Whether the alias was derived from the endpoints.
    pub alias_derived: bool,
    /// Full GTS instance id of the upstream.
    pub gts_id: String,
}

impl From<&Upstream> for UpstreamResponseDto {
    fn from(value: &Upstream) -> Self {
        Self {
            id: value.id.to_string(),
            enabled: value.enabled,
            alias: value.alias.clone(),
            tags: value.tags.clone(),
            server: ServerConfigDto::from(&value.server),
            protocol: value.protocol.into(),
            auth: value.auth.as_ref().map(AuthBindingDto::from),
            headers: value.headers.as_ref().map(HeaderRulesDto::from),
            plugins: value.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: value.rate_limit.as_ref().map(RateLimitConfigDto::from),
            cors: value.cors.as_ref().map(CorsConfigDto::from),
            alias_derived: value.alias_derived,
            gts_id: value.gts_id(),
        }
    }
}

impl TryFrom<&UpstreamRequestDto> for UpstreamDraft {
    type Error = DomainError;

    fn try_from(value: &UpstreamRequestDto) -> Result<Self, Self::Error> {
        Ok(Self {
            enabled: value.enabled,
            alias: value.alias.clone(),
            tags: value.tags.clone(),
            server: ServerConfig::try_from(&value.server)?,
            protocol: value.protocol.into(),
            auth: value.auth.as_ref().map(AuthBinding::from),
            headers: value.headers.as_ref().map(HeaderRules::from),
            plugins: value.plugins.as_ref().map(PluginChain::from),
            rate_limit: value.rate_limit.as_ref().map(RateLimitConfig::from),
            cors: value.cors.as_ref().map(CorsConfig::from),
        })
    }
}

/// Response body for `GET /oagw/v1/upstreams`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamListDto {
    /// The requested page of upstreams.
    pub items: Vec<UpstreamResponseDto>,
    /// Number of upstreams matching the filter, before pagination.
    pub total: u64,
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// HTTP method on the wire (uppercase).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[toolkit_macros::api_dto(response, request)]
pub enum HttpMethodDto {
    /// `GET`
    #[serde(rename = "GET")]
    Get,
    /// `POST`
    #[serde(rename = "POST")]
    Post,
    /// `PUT`
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`
    #[serde(rename = "DELETE")]
    Delete,
    /// `PATCH`
    #[serde(rename = "PATCH")]
    Patch,
}

impl From<crate::domain::model::route::HttpMethod> for HttpMethodDto {
    fn from(value: crate::domain::model::route::HttpMethod) -> Self {
        match value {
            crate::domain::model::route::HttpMethod::Get => Self::Get,
            crate::domain::model::route::HttpMethod::Post => Self::Post,
            crate::domain::model::route::HttpMethod::Put => Self::Put,
            crate::domain::model::route::HttpMethod::Delete => Self::Delete,
            crate::domain::model::route::HttpMethod::Patch => Self::Patch,
        }
    }
}

impl From<HttpMethodDto> for crate::domain::model::route::HttpMethod {
    fn from(value: HttpMethodDto) -> Self {
        match value {
            HttpMethodDto::Get => Self::Get,
            HttpMethodDto::Post => Self::Post,
            HttpMethodDto::Put => Self::Put,
            HttpMethodDto::Delete => Self::Delete,
            HttpMethodDto::Patch => Self::Patch,
        }
    }
}

/// `path_suffix_mode` on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum PathSuffixModeDto {
    /// Reject any request that carries a path suffix.
    Disabled,
    /// Append the suffix to `match.http.path`.
    #[default]
    Append,
}

impl From<crate::domain::model::route::PathSuffixMode> for PathSuffixModeDto {
    fn from(value: crate::domain::model::route::PathSuffixMode) -> Self {
        match value {
            crate::domain::model::route::PathSuffixMode::Disabled => Self::Disabled,
            crate::domain::model::route::PathSuffixMode::Append => Self::Append,
        }
    }
}

impl From<PathSuffixModeDto> for crate::domain::model::route::PathSuffixMode {
    fn from(value: PathSuffixModeDto) -> Self {
        match value {
            PathSuffixModeDto::Disabled => Self::Disabled,
            PathSuffixModeDto::Append => Self::Append,
        }
    }
}

/// `match.http` on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct HttpMatchDto {
    /// Methods this route serves.
    #[serde(default)]
    pub methods: Vec<HttpMethodDto>,
    /// Path pattern.
    #[serde(default)]
    pub path: String,
    /// Allowed query parameters; an empty list allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the captured `{path_suffix}` is handled.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixModeDto,
}

impl From<&HttpMatch> for HttpMatchDto {
    fn from(value: &HttpMatch) -> Self {
        Self {
            methods: value.methods.iter().map(|m| (*m).into()).collect(),
            path: value.path.clone(),
            query_allowlist: value.query_allowlist.clone(),
            path_suffix_mode: value.path_suffix_mode.into(),
        }
    }
}

impl From<&HttpMatchDto> for HttpMatch {
    fn from(value: &HttpMatchDto) -> Self {
        Self {
            methods: value.methods.iter().map(|m| (*m).into()).collect(),
            path: value.path.clone(),
            query_allowlist: value.query_allowlist.clone(),
            path_suffix_mode: value.path_suffix_mode.into(),
        }
    }
}

/// `match.grpc` on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct GrpcMatchDto {
    /// Fully qualified gRPC service name.
    #[serde(default)]
    pub service: String,
    /// RPC method name.
    #[serde(default)]
    pub method: String,
}

impl From<&GrpcMatch> for GrpcMatchDto {
    fn from(value: &GrpcMatch) -> Self {
        Self {
            service: value.service.clone(),
            method: value.method.clone(),
        }
    }
}

impl From<&GrpcMatchDto> for GrpcMatch {
    fn from(value: &GrpcMatchDto) -> Self {
        Self {
            service: value.service.clone(),
            method: value.method.clone(),
        }
    }
}

/// `match` on the wire (exactly one of `http`/`grpc`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum RouteMatchDto {
    /// HTTP matching.
    Http(HttpMatchDto),
    /// gRPC matching.
    Grpc(GrpcMatchDto),
}

impl Default for RouteMatchDto {
    fn default() -> Self {
        Self::Http(HttpMatchDto::default())
    }
}

impl From<&RouteMatch> for RouteMatchDto {
    fn from(value: &RouteMatch) -> Self {
        match value {
            RouteMatch::Http(http) => Self::Http(HttpMatchDto::from(http)),
            RouteMatch::Grpc(grpc) => Self::Grpc(GrpcMatchDto::from(grpc)),
        }
    }
}

impl From<&RouteMatchDto> for RouteMatch {
    fn from(value: &RouteMatchDto) -> Self {
        match value {
            RouteMatchDto::Http(http) => RouteMatch::Http(HttpMatch::from(http)),
            RouteMatchDto::Grpc(grpc) => RouteMatch::Grpc(GrpcMatch::from(grpc)),
        }
    }
}

/// Request body for `POST /oagw/v1/routes`.
///
/// `upstream_id` is required on create.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct RouteRequestDto {
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Referenced upstream.
    #[serde(default)]
    pub upstream_id: String,
    /// Protocol-scoped matching rules.
    #[serde(default, rename = "match", alias = "match_config")]
    pub match_config: RouteMatchDto,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Route-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
}

/// Request body for `PUT /oagw/v1/routes/{id}`.
///
/// `upstream_id` is **optional**: it is immutable, so repeating the current
/// value is accepted, a different value is rejected with 400, and omitting it
/// keeps the existing binding (DESIGN.md "PUT (Replace)").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct RouteUpdateDto {
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Present only when the client repeats the current binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Protocol-scoped matching rules.
    #[serde(default, rename = "match", alias = "match_config")]
    pub match_config: RouteMatchDto,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Route-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
}

/// Response body for the route endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteResponseDto {
    /// Server-generated id.
    pub id: String,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Referenced upstream.
    pub upstream_id: String,
    /// Protocol-scoped matching rules.
    #[serde(rename = "match")]
    pub match_config: RouteMatchDto,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Route-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfigDto>,
    /// Full GTS instance id of the route.
    pub gts_id: String,
}

impl From<&Route> for RouteResponseDto {
    fn from(value: &Route) -> Self {
        Self {
            id: value.id.to_string(),
            enabled: value.enabled,
            tags: value.tags.clone(),
            upstream_id: value.upstream_id.to_string(),
            match_config: RouteMatchDto::from(&value.match_config),
            plugins: value.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: value.rate_limit.as_ref().map(RateLimitConfigDto::from),
            gts_id: value.gts_id(),
        }
    }
}

/// Response body for `GET /oagw/v1/routes`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteListDto {
    /// The requested page of routes.
    pub items: Vec<RouteResponseDto>,
    /// Number of routes matching the filter, before pagination.
    pub total: u64,
}

/// Parse a resource id carried on the wire as a bare UUID or a GTS instance id.
#[must_use]
pub fn parse_resource_id(value: &str) -> Option<Uuid> {
    crate::domain::gts_helpers::parse_gts_instance_id(value)
}

/// Parse a required resource id, rejecting anything that is neither a bare
/// UUID nor a GTS instance id.
///
/// # Errors
/// [`DomainError::Validation`] naming `field`.
pub fn require_resource_id(value: &str, field: &str) -> Result<Uuid, DomainError> {
    parse_resource_id(value).ok_or_else(|| {
        DomainError::validation(field, format!("`{value}` is not a valid resource id"))
    })
}

impl TryFrom<&RouteRequestDto> for RouteDraft {
    type Error = DomainError;

    fn try_from(value: &RouteRequestDto) -> Result<Self, Self::Error> {
        Ok(Self {
            enabled: value.enabled,
            tags: value.tags.clone(),
            upstream_id: Some(require_resource_id(&value.upstream_id, "upstream_id")?),
            match_config: RouteMatch::from(&value.match_config),
            plugins: value.plugins.as_ref().map(PluginChain::from),
            rate_limit: value.rate_limit.as_ref().map(RateLimitConfig::from),
        })
    }
}

impl TryFrom<&RouteUpdateDto> for RouteDraft {
    type Error = DomainError;

    fn try_from(value: &RouteUpdateDto) -> Result<Self, Self::Error> {
        Ok(Self {
            enabled: value.enabled,
            tags: value.tags.clone(),
            upstream_id: match &value.upstream_id {
                None => None,
                Some(raw) => Some(require_resource_id(raw, "upstream_id")?),
            },
            match_config: RouteMatch::from(&value.match_config),
            plugins: value.plugins.as_ref().map(PluginChain::from),
            rate_limit: value.rate_limit.as_ref().map(RateLimitConfig::from),
        })
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Plugin kind on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum PluginKindDto {
    /// Credential injection.
    #[default]
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request/response mutation.
    Transform,
}

impl From<PluginKind> for PluginKindDto {
    fn from(value: PluginKind) -> Self {
        match value {
            PluginKind::Auth => Self::Auth,
            PluginKind::Guard => Self::Guard,
            PluginKind::Transform => Self::Transform,
        }
    }
}

impl From<PluginKindDto> for PluginKind {
    fn from(value: PluginKindDto) -> Self {
        match value {
            PluginKindDto::Auth => PluginKind::Auth,
            PluginKindDto::Guard => PluginKind::Guard,
            PluginKindDto::Transform => PluginKind::Transform,
        }
    }
}

/// How plugin content is delivered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub enum PluginSourceKindDto {
    /// Inline source held with the plugin record.
    #[default]
    Inline,
    /// Content referenced from an external location.
    Reference,
}

impl From<PluginSource> for PluginSourceKindDto {
    fn from(value: PluginSource) -> Self {
        match value {
            PluginSource::Inline => Self::Inline,
            PluginSource::Reference => Self::Reference,
        }
    }
}

impl From<PluginSourceKindDto> for PluginSource {
    fn from(value: PluginSourceKindDto) -> Self {
        match value {
            PluginSourceKindDto::Inline => PluginSource::Inline,
            PluginSourceKindDto::Reference => PluginSource::Reference,
        }
    }
}

/// Declared source content of a plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response, request)]
pub struct PluginSourceDto {
    /// How the content is delivered.
    #[serde(default, rename = "kind", alias = "type")]
    pub kind: PluginSourceKindDto,
    /// Inline source text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Language of the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// External location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl From<&PluginSourceRecord> for PluginSourceDto {
    fn from(value: &PluginSourceRecord) -> Self {
        Self {
            kind: value.kind.clone().into(),
            source_code: value.source_code.clone(),
            language: value.language.clone(),
            location: value.location.clone(),
        }
    }
}

impl From<&PluginSourceDto> for PluginSourceRecord {
    fn from(value: &PluginSourceDto) -> Self {
        Self {
            kind: value.kind.into(),
            source_code: value.source_code.clone(),
            language: value.language.clone(),
            location: value.location.clone(),
        }
    }
}

/// Request body for `POST /oagw/v1/plugins`.
#[derive(Debug, Clone, Default, PartialEq)]
#[toolkit_macros::api_dto(request)]
pub struct PluginRequestDto {
    /// Whether the plugin may be bound.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Operator-facing name.
    #[serde(default)]
    pub name: String,
    /// Operator-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which of the three plugin kinds this is.
    #[serde(default, rename = "type", alias = "plugin_type")]
    pub plugin_type: PluginKindDto,
    /// Implementation identifier of the backing executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
    /// Sharing mode.
    #[serde(default)]
    pub sharing: OagwSharingModeDto,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Default configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// Declared configuration schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Phases the plugin participates in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    /// Declared source content.
    #[serde(default)]
    pub source: PluginSourceDto,
}

impl From<&PluginRequestDto> for PluginDraft {
    fn from(value: &PluginRequestDto) -> Self {
        Self {
            enabled: value.enabled,
            name: value.name.clone(),
            description: value.description.clone(),
            plugin_type: value.plugin_type.into(),
            implementation: value.implementation.clone(),
            sharing: value.sharing.into(),
            tags: value.tags.clone(),
            config: value.config.clone(),
            config_schema: value.config_schema.clone(),
            phases: value.phases.clone(),
            source: PluginSourceRecord::from(&value.source),
        }
    }
}

/// Response body for the plugin endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginResponseDto {
    /// Server-generated id.
    pub id: String,
    /// Whether the plugin may be bound.
    pub enabled: bool,
    /// Operator-facing name.
    pub name: String,
    /// Operator-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Which of the three plugin kinds this is.
    #[serde(rename = "type")]
    pub plugin_type: PluginKindDto,
    /// Implementation identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
    /// How the plugin participates in tenant-hierarchy merging.
    pub sharing: OagwSharingModeDto,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Default configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    /// Declared configuration schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Phases the plugin participates in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    /// Full GTS instance id of the plugin.
    pub gts_id: String,
}

impl From<&Plugin> for PluginResponseDto {
    fn from(value: &Plugin) -> Self {
        Self {
            id: value.id.to_string(),
            enabled: value.enabled,
            name: value.name.clone(),
            description: value.description.clone(),
            plugin_type: value.plugin_type.into(),
            implementation: value.implementation.clone(),
            sharing: value.sharing.into(),
            tags: value.tags.clone(),
            config: value.config.clone(),
            config_schema: value.config_schema.clone(),
            phases: value.phases.clone(),
            gts_id: value.gts_id(),
        }
    }
}

/// Response body for `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceResponseDto {
    /// GTS instance id of the plugin.
    pub gts_id: String,
    /// How the content is delivered.
    #[serde(default, rename = "kind", alias = "type")]
    pub kind: PluginSourceKindDto,
    /// Inline source text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Language of the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// External location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl From<&Plugin> for PluginSourceResponseDto {
    fn from(value: &Plugin) -> Self {
        let source = PluginSourceDto::from(&value.source);
        Self {
            gts_id: value.gts_id(),
            kind: source.kind,
            source_code: source.source_code,
            language: source.language,
            location: source.location,
        }
    }
}

/// Response body for `GET /oagw/v1/plugins`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginListDto {
    /// The requested page of plugins.
    pub items: Vec<PluginResponseDto>,
    /// Number of plugins matching the filter, before pagination.
    pub total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_round_trips_the_schema_shape() {
        let raw = serde_json::json!({
            "enabled": true,
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        });
        let dto: UpstreamRequestDto = serde_json::from_value(raw).unwrap();
        assert_eq!(dto.server.endpoints.len(), 1);
        assert!(dto.alias.is_none());
        assert_eq!(dto.protocol, ProtocolDto::Http);
    }

    #[test]
    fn http_is_a_legal_scheme_unconditionally() {
        let raw = serde_json::json!({
            "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.1", "port": 8080}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        });
        let dto: UpstreamRequestDto = serde_json::from_value(raw).unwrap();
        assert_eq!(dto.server.endpoints[0].scheme, SchemeDto::Http);
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        let raw = serde_json::json!({"protocol": "gts.cf.core.oagw.protocol.v1~bogus.v1"});
        assert!(serde_json::from_value::<UpstreamRequestDto>(raw).is_err());
    }

    #[test]
    fn route_match_uses_the_match_key() {
        let raw = serde_json::json!({
            "upstream_id": uuid::Uuid::new_v4().to_string(),
            "match": {"http": {"methods": ["GET", "POST"], "path": "/v1/chat"}}
        });
        let dto: RouteRequestDto = serde_json::from_value(raw).unwrap();
        match &dto.match_config {
            RouteMatchDto::Http(http) => {
                assert_eq!(http.methods.len(), 2);
                assert_eq!(http.path, "/v1/chat");
                assert_eq!(http.path_suffix_mode, PathSuffixModeDto::Append);
            }
            other => panic!("expected an http match, got {other:?}"),
        }
    }

    #[test]
    fn grpc_match_is_accepted() {
        let raw = serde_json::json!({
            "upstream_id": uuid::Uuid::new_v4().to_string(),
            "match": {"grpc": {"service": "foo.v1.UserService", "method": "GetUser"}}
        });
        let dto: RouteRequestDto = serde_json::from_value(raw).unwrap();
        assert!(matches!(dto.match_config, RouteMatchDto::Grpc(_)));
    }

    #[test]
    fn plugin_bindings_accept_strings_and_objects() {
        let chain: PluginChainDto = serde_json::from_value(serde_json::json!({
            "sharing": "inherit",
            "items": [
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                {"plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                 "config": {"required_request_headers": "x-correlation-id"}}
            ]
        }))
        .unwrap();
        assert_eq!(chain.items.len(), 2);
        assert_eq!(chain.sharing, OagwSharingModeDto::Inherit);
    }

    #[test]
    fn plugin_response_uses_the_type_field() {
        let plugin = Plugin::default();
        let body = serde_json::to_value(PluginResponseDto::from(&plugin)).unwrap();
        assert_eq!(body["type"], "transform");
        assert!(body.get("plugin_type").is_none());
    }

    #[test]
    fn plugin_request_accepts_plugin_type_alias() {
        let dto: PluginRequestDto =
            serde_json::from_value(serde_json::json!({"plugin_type": "guard", "name": "x"}))
                .unwrap();
        assert_eq!(dto.plugin_type, PluginKindDto::Guard);
        let dto: PluginRequestDto =
            serde_json::from_value(serde_json::json!({"type": "transform", "name": "x"})).unwrap();
        assert_eq!(dto.plugin_type, PluginKindDto::Transform);
    }
}
