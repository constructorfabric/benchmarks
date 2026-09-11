//! Domain types mirroring `docs/schemas/*.v1.schema.json`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;


/// Endpoint schemes the gateway accepts. `http` is only legal when the gear config sets
/// `allow_http_upstream: true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scheme {
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "https")]
    Https,
    #[serde(rename = "wss")]
    Wss,
    #[serde(rename = "wt")]
    Wt,
    #[serde(rename = "grpc")]
    Grpc,
}

impl Scheme {
    /// Port used when the endpoint does not name one.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// True for the schemes that speak plain HTTP/1.1 or h2 without TLS.
    #[must_use]
    pub fn is_plain_http(self) -> bool {
        matches!(self, Self::Http)
    }

    /// True for the schemes that carry WebSocket traffic.
    #[must_use]
    pub fn is_websocket(self) -> bool {
        matches!(self, Self::Wss)
    }

    /// Wire name of the scheme.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// One upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub scheme: Scheme,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Host, or `host:port` when a non-default port is configured.
    #[must_use]
    pub fn authority(&self) -> String {
        match self.port {
            Some(p) if p != self.scheme.default_port() => format!("{}:{p}", self.host),
            _ => self.host.clone(),
        }
    }

    /// Host with an explicit port, for building an origin.
    #[must_use]
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.authority())
    }

    /// Effective port of the endpoint.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }
}

/// `server` block of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical sharing semantics, per the schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SharingMode {
    #[serde(rename = "private")]
    #[default]
    Private,
    #[serde(rename = "inherit")]
    Inherit,
    #[serde(rename = "enforce")]
    Enforce,
}

/// Header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HeadersConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// Rules applied to the outbound request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RequestHeaderRules {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<PassthroughMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Rules applied to the response returned to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResponseHeaderRules {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Inbound request header forwarding policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PassthroughMode {
    #[serde(rename = "none")]
    #[default]
    None,
    #[serde(rename = "allowlist")]
    Allowlist,
    #[serde(rename = "all")]
    All,
}

/// Rate limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_cost")]
    pub cost: u32,
}

fn default_cost() -> u32 {
    1
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: None,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }
}

/// Rate limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RateAlgorithm {
    #[serde(rename = "token_bucket")]
    #[default]
    TokenBucket,
    #[serde(rename = "sliding_window")]
    SlidingWindow,
}

/// Sustained rate of a limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    pub rate: u32,
    #[serde(default)]
    pub window: RateWindow,
}

/// Window of a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RateWindow {
    #[serde(rename = "second")]
    Second,
    #[serde(rename = "minute")]
    #[default]
    Minute,
    #[serde(rename = "hour")]
    Hour,
    #[serde(rename = "day")]
    Day,
}

impl RateWindow {
    /// Window length in seconds.
    #[must_use]
    pub fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Burst (bucket capacity) of a limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstConfig {
    pub capacity: u32,
}

/// Key the rate limit counters are keyed by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RateLimitScope {
    #[serde(rename = "global")]
    Global,
    #[serde(rename = "tenant")]
    #[default]
    Tenant,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "ip")]
    Ip,
    #[serde(rename = "route")]
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RateLimitStrategy {
    #[serde(rename = "reject")]
    #[default]
    Reject,
    #[serde(rename = "queue")]
    Queue,
    #[serde(rename = "degrade")]
    Degrade,
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_string(), "POST".to_string()]
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: None,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_allowed_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// Auth plugin binding on an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(rename = "type")]
    pub plugin_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// A plugin reference: either a bare identifier (GTS id or plugin UUID) or an object naming the
/// plugin and its per-binding configuration, as ADR-0009 shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginRef {
    Bare(String),
    Detailed { plugin_ref: String, #[serde(default, skip_serializing_if = "Option::is_none")] config: Option<serde_json::Map<String, serde_json::Value>> },
}

/// `plugins` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginRef>,
}

/// Protocol-scoped match rules of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteMatch {
    Http(HttpMatch),
    Grpc(GrpcMatch),
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMatch {
    pub methods: Vec<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How the proxy URL's path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PathSuffixMode {
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "append")]
    #[default]
    Append,
}

/// gRPC match rules (validated only; DESIGN §3.1 defers gRPC proxying).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

impl RouteMatch {
    /// The HTTP match rules, when the route is HTTP-scoped.
    #[must_use]
    pub fn as_http(&self) -> Option<&HttpMatch> {
        match self {
            Self::Http(m) => Some(m),
            Self::Grpc(_) => None,
        }
    }
}

/// A stored upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub alias: String,
    #[serde(default = "crate::domain::validation::default_true")]
    pub enabled: bool,
    pub protocol: String,
    pub server: ServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A stored route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub upstream_id: Uuid,
    #[serde(default = "crate::domain::validation::default_true")]
    pub enabled: bool,
    pub route_match: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A stored (custom) plugin definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
}

/// An upstream, its resolved tenant chain and the route that matched.
#[derive(Debug, Clone)]
pub struct ResolvedRoute<'a> {
    /// Upstream the route belongs to.
    pub upstream: &'a Upstream,
    /// Route that matched, if any.
    pub route: Option<&'a Route>,
}
