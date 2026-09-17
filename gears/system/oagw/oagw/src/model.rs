//! Domain models mirroring `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` (plus the plugin CRUD model).
//!
//! These are the stored representations and the management-API response
//! bodies. Request bodies live in [`crate::api::dto`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use toolkit_macros::api_dto;
use utoipa::ToSchema;
use uuid::Uuid;

/// Sharing mode for hierarchical configuration.
///
/// * `private` — not visible to descendants
/// * `inherit` — descendants can override
/// * `enforce` — descendants cannot override
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    #[default]
    Private,
    Inherit,
    Enforce,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Allows bursts.
    #[default]
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    #[default]
    Global,
    Tenant,
    User,
    Ip,
    Route,
}

/// Behavior when the rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Time window for a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateLimitWindow {
    /// Window length in seconds.
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Inbound header forwarding policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPassthrough {
    /// Forward no inbound headers.
    #[default]
    None,
    /// Forward only the allowlisted headers.
    Allowlist,
    /// Forward all inbound headers.
    All,
}

/// Route path-suffix handling mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject path-suffix usage.
    Disabled,
    /// Append the suffix to the matched path.
    #[default]
    Append,
}

/// One upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Endpoint {
    /// `https`, `wss`, `wt`, or `grpc`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname or IP address of the upstream service.
    pub host: String,
    /// Port (default 443; http defaults to 80).
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_scheme() -> String {
    "https".to_string()
}

impl Endpoint {
    /// Effective port for this endpoint.
    pub fn effective_port(&self) -> u16 {
        self.port
            .unwrap_or_else(|| if self.scheme == "http" { 80 } else { 443 })
    }

    /// URL scheme usable for proxying (http/https are proxied; the rest are
    /// catalog-only and rejected with a protocol error).
    pub fn url_scheme(&self) -> Option<&str> {
        match self.scheme.as_str() {
            "https" => Some("https"),
            "http" => Some("http"),
            _ => None,
        }
    }
}

/// Upstream server descriptor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UpstreamServer {
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin configuration for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthConfig {
    /// Auth plugin type (GTS identifier, e.g. `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    #[serde(rename = "type")]
    pub auth_type: String,
    #[serde(default)]
    pub sharing: SharingMode,
    /// Auth plugin configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if exists).
    #[serde(default)]
    pub set: HashMap<String, String>,
    /// Headers to add (append, allow duplicates).
    #[serde(default)]
    pub add: HashMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: HeaderPassthrough,
    /// Headers to forward when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    #[serde(default)]
    pub set: HashMap<String, String>,
    /// Headers to add to the response.
    #[serde(default)]
    pub add: HashMap<String, String>,
    /// Headers to strip from the upstream response.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation rules for requests/responses.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeaderRules,
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

/// Sustained component of a rate limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Time window. Defaults to `second`.
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// Burst component of a rate limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BurstConfig {
    /// Maximum burst size (bucket capacity). Defaults to `sustained.rate`.
    pub capacity: u64,
}

/// Rate limiting configuration (upstream or route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
    /// Include `X-RateLimit-*` response headers.
    #[serde(default)]
    pub response_headers: Option<bool>,
}

fn default_cost() -> u64 {
    1
}

impl RateLimitConfig {
    /// Burst capacity, defaulting to the sustained rate.
    pub fn burst_capacity(&self) -> u64 {
        self.burst.as_ref().map(|b| b.capacity).unwrap_or(self.sustained.rate)
    }

    /// Whether `X-RateLimit-*` response headers are included (default true).
    pub fn include_response_headers(&self) -> bool {
        self.response_headers.unwrap_or(true)
    }
}

/// CORS configuration (upstream or route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS for this resource (disabled by default).
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_string(), "POST".to_string()]
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// Ordered plugin bindings with a sharing mode.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    /// Builtin plugins referenced by GTS identifier, custom plugins by UUID
    /// (as a bare UUID string or a UUID-backed GTS identifier).
    #[serde(default)]
    pub items: Vec<String>,
}

/// HTTP route match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HttpMatch {
    /// Supported HTTP methods.
    pub methods: Vec<String>,
    /// Path pattern for the route.
    pub path: String,
    /// Query parameters forwardable to the upstream. Empty → allow none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How to treat `/{path_suffix}` from the proxy URL.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC route match rules (catalog-only; no gRPC proxy code path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Protocol-scoped route match. Exactly one of `http`/`grpc` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RouteMatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A single upstream service.
#[derive(Debug, Clone, PartialEq, Eq)]
#[api_dto(response)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub enabled: bool,
    pub alias: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Upstream {
    /// Whether the upstream uses an HTTP protocol (vs gRPC).
    pub fn is_http(&self) -> bool {
        crate::alias::is_grpc_protocol(&self.protocol) != true
    }
}

/// A single route bound to an upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[api_dto(response)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    #[serde(default)]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub r#match: RouteMatch,
    pub plugins: PluginsConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// Custom (tenant-defined) plugin. Plugins are immutable after creation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[api_dto(response)]
pub struct CustomPlugin {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    /// Full plugin type: one of the OAGW plugin base types (e.g.
    /// `gts.cf.core.oagw.guard_plugin.v1~...`).
    pub plugin_type: String,
    /// Plugin configuration instance (validated against the type catalog).
    #[serde(default)]
    pub config: serde_json::Value,
    /// Starlark source (custom plugins), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}
