//! OAGW domain models.
//!
//! The wire shape mirrors `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` (serde defaults, `deny_unknown_fields`),
//! plus the plugin model from DESIGN §3.1. `id`/`tenant_id` are server-owned
//! and never accepted from the client; `tenant_id` is carried on the stored
//! wrapper so it never leaks into serialized responses.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// GTS protocol identifiers (upstream.v1.schema.json `protocol` enum).
pub mod protocol_gts {
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`
    pub const HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`
    pub const GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";
}

/// GTS plugin base-type identifiers (DESIGN §3.1).
pub mod plugin_gts {
    /// `gts.cf.core.oagw.auth_plugin.v1~`
    pub const AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~";
    /// `gts.cf.core.oagw.guard_plugin.v1~`
    pub const GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~";
    /// `gts.cf.core.oagw.transform_plugin.v1~`
    pub const TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~";

    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`
    pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`
    pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`
    pub const AUTH_OAUTH2_CLIENT_CRED: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1`
    pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`
    pub const GUARD_REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1`
    pub const TRANSFORM_REQUEST_ID: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

    /// Extract the instance segment after `~` from a plugin GTS identifier.
    pub fn instance_of(plugin_ref: &str) -> &str {
        plugin_ref.rsplit('~').next().unwrap_or(plugin_ref)
    }

    /// Whether `plugin_ref` parses (after `~`) as a plugin UUID.
    pub fn is_uuid_backed(plugin_ref: &str) -> bool {
        super::Uuid::parse_str(instance_of(plugin_ref)).is_ok()
    }
}

/// Sharing mode for hierarchical configuration (upstream.v1.schema.json).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; descendants can override.
    Inherit,
    /// Visible; descendants cannot override.
    Enforce,
}

/// Endpoint scheme (upstream.v1.schema.json `scheme` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    #[default]
    Https,
    Wss,
    Wt,
    Grpc,
    #[serde(rename = "http")]
    Http,
}

impl Scheme {
    /// Map the scheme to an HTTP client scheme for the forwarder. Only
    /// `http`/`https` have a real transport here; `wss`/`wt`/`grpc` tunnel
    /// over TLS on the same authority.
    #[must_use]
    pub fn http_scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => "https",
        }
    }

    /// Standard default port for this scheme.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

/// Upstream endpoint (scheme/host/port).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Endpoint {
    /// Scheme — `https` by default; `http` requires `allow_http_upstream`.
    pub scheme: Scheme,
    /// Hostname, IPv4 or IPv6 of the upstream service.
    pub host: String,
    /// Port (default: per-scheme standard port).
    pub port: u16,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            scheme: Scheme::Https,
            host: String::new(),
            port: 443,
        }
    }
}

/// Server configuration (one or more endpoints forming a load-balance pool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
        }
    }
}

/// Authentication configuration for an upstream.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier (e.g. `...~cf.core.oagw.apikey.v1`).
    pub plugin_type: Option<String>,
    /// Auth plugin configuration (plugin-specific keys).
    pub config: serde_json::Value,
    /// Sharing mode for the auth config.
    pub sharing: SharingMode,
}

impl AuthConfig {
    /// Resolved plugin type (falls back to the no-op plugin when absent).
    #[must_use]
    pub fn resolved_type(&self) -> &str {
        self.plugin_type.as_deref().unwrap_or(plugin_gts::AUTH_NOOP)
    }
}

/// Header transformation rules (upstream.v1.schema.json `definitions.headers`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HeadersTransform {
    /// Headers to set (overwrite).
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append, allow duplicates).
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to remove.
    pub remove: Vec<String>,
}

/// Inbound passthrough policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound headers.
    #[default]
    None,
    /// Forward only the allowlisted headers.
    Allowlist,
    /// Forward all inbound headers.
    All,
}

/// Request header transformation rules.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RequestHeadersConfig {
    /// Headers to set on the outbound request.
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add on the outbound request.
    pub add: std::collections::BTreeMap<String, String>,
    /// Inbound header names to remove.
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough == allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResponseHeadersConfig {
    /// Headers to set on the response to the client.
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add on the response to the client.
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    pub remove: Vec<String>,
}

/// Full header configuration for an upstream.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HeadersConfig {
    pub request: RequestHeadersConfig,
    pub response: ResponseHeadersConfig,
}

/// Rate-limit time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Window {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl Window {
    /// Window length in seconds.
    #[must_use]
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained rate (tokens replenished per window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SustainedRate {
    /// Tokens replenished per window (minimum 1).
    pub rate: u64,
    /// Time window (default `second`).
    pub window: Window,
}

impl Default for SustainedRate {
    fn default() -> Self {
        Self {
            rate: 1,
            window: Window::Second,
        }
    }
}

/// Burst capacity (bucket capacity; defaults to `sustained.rate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct BurstCap {
    pub capacity: u64,
}

impl BurstCap {
    /// Resolve the effective bucket capacity (defaults to sustained rate).
    #[must_use]
    pub fn capacity_or(&self, sustained: u64) -> u64 {
        if self.capacity == 0 {
            sustained
        } else {
            self.capacity
        }
    }
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RlAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Rate-limit scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RlScope {
    #[default]
    Global,
    Tenant,
    User,
    Ip,
    Route,
}

/// Rate-limit strategy when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RlStrategy {
    /// Reject with 429 + Retry-After.
    #[default]
    Reject,
    /// Queue within bounded capacity (not implemented; treated as reject).
    Queue,
    /// Process with reduced functionality (not implemented; treated as reject).
    Degrade,
}

/// Rate limiting configuration (upstream.v1.schema.json `definitions.rate_limit`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RateLimitConfig {
    /// Sharing mode; `enforce` descendants cannot exceed this limit.
    pub sharing: SharingMode,
    /// Rate limiting algorithm (token bucket implemented; sliding window rejected).
    pub algorithm: RlAlgorithm,
    /// Sustained rate (required by the schema — but optional here so
    /// `PartialEq`/`Default` derive cleanly; the schema's requiredness is
    /// enforced in validation).
    pub sustained: Option<SustainedRate>,
    /// Burst capacity (defaults to `sustained.rate`).
    pub burst: Option<BurstCap>,
    /// Scope for rate-limit counters.
    pub scope: RlScope,
    /// Behavior when the limit is exceeded.
    pub strategy: RlStrategy,
    /// Tokens consumed per request.
    pub cost: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            algorithm: RlAlgorithm::TokenBucket,
            sustained: None,
            burst: None,
            scope: RlScope::Tenant,
            strategy: RlStrategy::Reject,
            cost: 1,
        }
    }
}

/// CORS configuration (upstream.v1.schema.json `definitions.cors`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CorsConfig {
    pub sharing: SharingMode,
    /// Enables CORS handling for this upstream/route.
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin (forbidden with credentials).
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    pub expose_headers: Vec<String>,
    /// Allow credentials; requires specific (non-`*`) origins.
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// Plugin binding list with a sharing mode.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PluginsConfig {
    pub sharing: SharingMode,
    pub items: Vec<String>,
}

/// Upstream wire + stored record (mirrors upstream.v1.schema.json).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Upstream {
    /// Server-generated unique identifier (read-only; absent on create).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Whether the upstream is enabled (default true).
    pub enabled: bool,
    /// Routing identifier. Derivable aliases are enforced; `None` on create
    /// triggers auto-derivation when possible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags (additive across the tenant hierarchy).
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    /// Protocol GTS identifier (http/grpc).
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: protocol_gts::HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

/// HTTP route matching rules (route.v1.schema.json `definitions.http_match`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HttpMatch {
    /// Supported HTTP methods (min 1).
    pub methods: Vec<String>,
    /// Path prefix pattern for the route.
    pub path: String,
    /// Allowed query parameters; empty allows none.
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` from the proxy URL is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            path: String::new(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// Path-suffix handling for a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject `/{path_suffix}` usage.
    Disabled,
    /// Append the suffix to the matched path.
    #[default]
    Append,
}

/// gRPC route matching rules (route.v1.schema.json `definitions.grpc_match`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules — exactly one of `http`/`grpc` must be present.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MatchConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// Route wire + stored record (mirrors route.v1.schema.json).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Route {
    /// Server-generated unique identifier (read-only; absent on create).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Reference to the upstream this route belongs to (immutable after create).
    pub upstream_id: Uuid,
    /// Protocol-scoped match rules (`match` on the wire).
    #[serde(rename = "match")]
    pub match_: MatchConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            tags: Vec::new(),
            upstream_id: Uuid::nil(),
            match_: MatchConfig::default(),
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

/// Custom (UUID-backed) plugin stored in the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Plugin {
    /// Server-generated unique identifier (read-only; absent on create).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Plugin family: `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Tenant-unique plugin name.
    pub name: String,
    /// JSON Schema describing the accepted `config` object for this plugin.
    pub config_schema: serde_json::Value,
    /// Starlark source code (immutable after creation).
    pub source_code: String,
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: None,
            plugin_type: "guard".to_owned(),
            name: String::new(),
            config_schema: serde_json::Value::Object(Default::default()),
            source_code: String::new(),
        }
    }
}

impl Plugin {
    /// The GTS identifier for this custom plugin: `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
    #[must_use]
    pub fn gts_id(&self) -> Option<String> {
        self.id.map(|id| match self.plugin_type.as_str() {
            "auth" => format!("{}{id}", plugin_gts::AUTH),
            "transform" => format!("{}{id}", plugin_gts::TRANSFORM),
            _ => format!("{}{id}", plugin_gts::GUARD),
        })
    }

    /// Validate the plugin family.
    #[must_use]
    pub fn valid_type(&self) -> bool {
        matches!(self.plugin_type.as_str(), "auth" | "guard" | "transform")
    }
}

/// Stored wrapper carrying tenant scoping (never serialized to the client).
#[derive(Debug, Clone, PartialEq)]
pub struct Stored<T> {
    pub tenant_id: Uuid,
    pub record: T,
}

impl<T> Stored<T> {
    /// Create a stored wrapper.
    #[must_use]
    pub fn new(tenant_id: Uuid, record: T) -> Self {
        Self { tenant_id, record }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn upstream_from_schema_example() {
        let json = serde_json::json!({
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "api.openai.com", "port": 443 }
                ]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        });
        let up: Upstream = serde_json::from_value(json).expect("valid upstream");
        assert!(up.enabled);
        assert_eq!(up.server.endpoints.len(), 1);
        assert_eq!(up.server.endpoints[0].scheme, Scheme::Https);
    }

    #[test]
    fn route_match_requires_one_of_http_grpc() {
        let json = serde_json::json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let route: Route = serde_json::from_value(json).expect("valid route");
        assert!(route.match_.http.is_some());
        assert!(route.match_.grpc.is_none());
    }
}
