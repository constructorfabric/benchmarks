//! Domain model for OAGW resources.
//!
//! These types mirror the JSON schemas in `docs/schemas/` (upstream.v1 and
//! route.v1) plus the ADR configuration blocks (rate limiting ADR-0003, CORS
//! ADR-0004, plugin bindings ADR-0002/0009). The model is lenient on unknown
//! fields (so schema and ADR shapes both deserialize) but every recognized
//! key is validated by the control plane before a resource is accepted.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

/// Sharing mode for hierarchical configuration merge (DESIGN.md § hierarchy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Config is private to this resource.
    #[default]
    Private,
    /// Child inherits the effective parent value when it does not define its
    /// own; union semantics for lists, min for rates.
    Inherit,
    /// Parents constrain children; a child value is capped by the parent's.
    Enforce,
}

/// Upstream protocol. Serialized as the GTS identifier from the schema
/// (`gts.cf.core.oagw.protocol.v1~cf.core.oagw.{http,grpc}.v1`); friendly
/// short names are accepted on input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, utoipa::ToSchema)]
pub enum Protocol {
    Http,
    Grpc,
    // NOTE: websockets/webtransport are carried over the `http` protocol
    // family (scheme wss/wt) — they do not get their own protocol id.
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_gts())
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Protocol::from_gts(&s).map_err(serde::de::Error::custom)
    }
}

impl Protocol {
    /// GTS identifier of this protocol (DESIGN.md protocol table).
    #[must_use]
    pub fn as_gts(&self) -> &'static str {
        match self {
            Self::Http => crate::gts_helpers::PROTOCOL_HTTP_TYPE,
            Self::Grpc => crate::gts_helpers::PROTOCOL_GRPC_TYPE,
        }
    }

    /// Resolve a GTS identifier (or friendly short name) to a protocol.
    pub fn from_gts(s: &str) -> Result<Self, String> {
        match s {
            "http" | "rest" | crate::gts_helpers::PROTOCOL_HTTP_TYPE => Ok(Self::Http),
            "grpc" | "grpcs" | crate::gts_helpers::PROTOCOL_GRPC_TYPE => Ok(Self::Grpc),
            other => Err(format!(
                "unsupported protocol `{other}` (expected `{http}` or `{grpc}`)",
                http = crate::gts_helpers::PROTOCOL_HTTP_TYPE,
                grpc = crate::gts_helpers::PROTOCOL_GRPC_TYPE,
            )),
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http => write!(f, "http"),
            Self::Grpc => write!(f, "grpc"),
        }
    }
}

/// Header passthrough mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// No inbound headers forwarded (default).
    #[default]
    None,
    /// Only allowlisted inbound headers forwarded.
    Allowlist,
    /// All inbound headers forwarded.
    All,
}

/// Rate limit window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateLimitWindow {
    /// The window length in seconds.
    #[must_use]
    pub fn as_secs(&self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86400,
        }
    }
}

/// Rate limiting algorithm (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Dual-rate token bucket (default). Burst capacity above sustained rate.
    #[default]
    TokenBucket,
    /// Simple sliding window counter (sustained rate only).
    SlidingWindow,
}

/// Rate limit scope (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// Global across all tenants of this route/upstream.
    Global,
    /// Per calling tenant.
    #[default]
    Tenant,
    /// Per calling subject.
    User,
    /// Per caller IP address.
    Ip,
    /// Per matched route.
    Route,
}

impl RateLimitScope {
    /// Stable scope key prefix used in the limiter cache key.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Tenant => "tenant",
            Self::User => "user",
            Self::Ip => "ip",
            Self::Route => "route",
        }
    }
}

/// Rate limit strategy when the limit is exceeded (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with 429 and `Retry-After`.
    #[default]
    Reject,
    /// Queue until a token is available (bounded; see config).
    Queue,
    /// Allow the request but label it (rate limit not enforced).
    Degrade,
}

// ---------------------------------------------------------------------------
// Server / endpoint configuration
// ---------------------------------------------------------------------------

/// One upstream network endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// URL scheme: `https` (default), `wss`, `wt`, `grpc`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname (or IP) of the endpoint.
    pub host: String,
    /// Port; defaults to 443 for standard schemes.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_scheme() -> String {
    "https".to_owned()
}

fn default_port() -> u16 {
    443
}

/// `server.endpoints` configuration block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

// ---------------------------------------------------------------------------
// Auth configuration
// ---------------------------------------------------------------------------

/// `auth` configuration block. The GTS identifier of the auth plugin lives
/// under the wire key `type` (per upstream schema); `config` carries
/// plugin-specific keys.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin.
    #[serde(rename = "type")]
    pub auth_type: String,
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Headers configuration
// ---------------------------------------------------------------------------

/// Request header transformation block.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RequestHeadersConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "is_default_passthrough")]
    pub passthrough: PassthroughMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

fn is_default_passthrough(m: &PassthroughMode) -> bool {
    *m == PassthroughMode::None
}

/// Response header transformation block.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResponseHeadersConfig {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// `headers` configuration block.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeadersConfig,
    #[serde(default)]
    pub response: ResponseHeadersConfig,
}

// ---------------------------------------------------------------------------
// Plugin configuration
// ---------------------------------------------------------------------------

/// A plugin binding: either a bare GTS identifier (schema string form) or a
/// `{plugin_ref, config}` object (ADR-0009 example form). Both serialize back
/// to the form they were received in.
#[derive(Debug, Clone, PartialEq, utoipa::ToSchema)]
pub struct PluginBinding {
    /// Full GTS identifier of the plugin (named builtin or custom UUID).
    pub plugin_ref: String,
    /// Optional plugin-specific configuration.
    pub config: Option<serde_json::Value>,
}

impl PluginBinding {
    /// A binding without configuration.
    #[must_use]
    pub fn new(plugin_ref: impl Into<String>) -> Self {
        Self {
            plugin_ref: plugin_ref.into(),
            config: None,
        }
    }
}

impl Serialize for PluginBinding {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.config {
            None => serializer.serialize_newtype_struct("PluginBinding", &self.plugin_ref),
            Some(cfg) => {
                use serde::ser::SerializeStruct;
                let mut s = serializer.serialize_struct("PluginBinding", 2)?;
                s.serialize_field("plugin_ref", &self.plugin_ref)?;
                s.serialize_field("config", cfg)?;
                s.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Full {
            plugin_ref: String,
            #[serde(default)]
            config: Option<serde_json::Value>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Bare(String),
            Full(Full),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Bare(s) => Ok(Self {
                plugin_ref: s,
                config: None,
            }),
            Wire::Full(f) => Ok(Self {
                plugin_ref: f.plugin_ref,
                config: f.config,
            }),
        }
    }
}

/// `plugins` chain configuration (upstream or route level).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginChainConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

// ---------------------------------------------------------------------------
// Rate limit configuration (ADR-0003)
// ---------------------------------------------------------------------------

/// `rate_limit` configuration block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained request rate and window. `rate` is required (>= 1).
    pub sustained: SustainedRate,
    /// Burst allowance; `capacity` defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Per-request cost.
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Whether `X-RateLimit-*` response headers are emitted.
    #[serde(default = "default_response_headers")]
    pub response_headers: bool,
}

fn default_cost() -> u32 {
    1
}

fn default_response_headers() -> bool {
    true
}

/// Sustained rate: `rate` requests per `window`. `rate` is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SustainedRate {
    pub rate: u64,
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// Burst capacity above the sustained rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BurstConfig {
    #[serde(default)]
    pub capacity: u64,
}

// ---------------------------------------------------------------------------
// CORS configuration (ADR-0004)
// ---------------------------------------------------------------------------

/// `cors` configuration block.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub enabled: bool,
    /// List of allowed origins or `["*"]` (any origin).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

// ---------------------------------------------------------------------------
// Match configuration (route)
// ---------------------------------------------------------------------------

/// Route path suffix handling (route schema `path_suffix_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Path suffix matching disabled — route matches only the exact path.
    Disabled,
    /// Path suffix can be appended when the route matches (default).
    #[default]
    Append,
}

/// HTTP match rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpMatchConfig {
    #[serde(default)]
    pub methods: Vec<String>,
    #[serde(default)]
    pub path: String,
    /// Query parameters copied from the client request. Empty = allow none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rule (routes exist but the proxy data plane does not serve
/// gRPC traffic in this milestone).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrpcMatchConfig {
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub method: String,
}

/// A route match rule: exactly one of `http` or `grpc` (route schema
/// `match` block, `additionalProperties: false`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MatchConfig {
    Http(HttpMatchConfig),
    Grpc(GrpcMatchConfig),
}

impl MatchConfig {
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        matches!(self, Self::Grpc(_))
    }
}

// ---------------------------------------------------------------------------
// Plugin definitions (custom plugins registry)
// ---------------------------------------------------------------------------

/// Kind of a plugin (mirrors `plugin_type` in plugin CRUD payloads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    /// GTS type of plugin resources of this kind.
    #[must_use]
    pub fn resource_type(&self) -> &'static str {
        match self {
            Self::Auth => crate::gts_helpers::AUTH_PLUGIN_TYPE,
            Self::Guard => crate::gts_helpers::GUARD_PLUGIN_TYPE,
            Self::Transform => crate::gts_helpers::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Full GTS identifier for a plugin of this kind identified by `id`.
    #[must_use]
    pub fn gts_id_for(&self, id: Uuid) -> String {
        format!("{}~{}", self.resource_type(), id)
    }
}

/// A user-defined plugin created through the management API.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDef {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub plugin_type: PluginKind,
    /// Transform phases (e.g. `on_request`, `on_response`, `on_error`).
    pub phases: Vec<String>,
    pub config_schema: Option<serde_json::Value>,
    pub source_code: Option<String>,
}

impl PluginDef {
    /// Full GTS identifier referencing this custom plugin.
    #[must_use]
    pub fn gts_id(&self) -> String {
        self.plugin_type.gts_id_for(self.id)
    }
}

// ---------------------------------------------------------------------------
// Stored resources
// ---------------------------------------------------------------------------

/// A stored upstream resource.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub enabled: bool,
    /// Normalized alias (ASCII lowercase, trailing dots stripped).
    pub alias: String,
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    pub auth: Option<AuthConfig>,
    pub headers: HeadersConfig,
    pub plugins: PluginChainConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: CorsConfig,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A stored route resource.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub upstream_id: Uuid,
    pub enabled: bool,
    pub tags: Vec<String>,
    pub match_config: MatchConfig,
    pub plugins: PluginChainConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Route {
    #[must_use]
    pub fn match_config(&self) -> &MatchConfig {
        &self.match_config
    }
}

// ---------------------------------------------------------------------------
// Effective (merged) configuration for the data plane
// ---------------------------------------------------------------------------

/// Minimum of a non-empty set of rate configs: the config with the lowest
/// per-second sustained rate wins; ties preserve the first in the chain.
pub fn min_rate_limit<'a>(
    chain: impl IntoIterator<Item = &'a RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let mut iter = chain.into_iter();
    let mut best = iter.next()?.clone();
    let best_eps = eps(&best);
    for next in iter {
        if eps(next) < best_eps {
            best = next.clone();
        }
    }
    Some(best)
}

fn eps(cfg: &RateLimitConfig) -> f64 {
    cfg.sustained.rate as f64 / cfg.sustained.window.as_secs() as f64
}
