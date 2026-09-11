//! Domain model for OAGW configuration: upstreams, routes and plugins.
//!
//! These types mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`. GTS identifiers are carried as plain
//! strings — the schema, not the type system, constrains their shape.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Canonical GTS base identifiers for the OAGW protocol and plugin families.
pub mod gts {
    /// Base type for upstream resources.
    pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
    /// Base type for route resources.
    pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
    /// Base type for auth plugins.
    pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
    /// Base type for guard plugins.
    pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
    /// Base type for transform plugins.
    pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
    /// HTTP protocol identifier.
    pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    /// gRPC protocol identifier (catalogued; not proxied in this phase).
    pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";
}

/// Whether a resource is visible to descendant tenants and how they may
/// override it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    #[serde(rename = "private")]
    Private,
    /// Visible; descendants may override.
    #[serde(rename = "inherit")]
    Inherit,
    /// Visible; descendants may not override.
    #[serde(rename = "enforce")]
    Enforce,
}

impl SharingMode {
    /// Parse the wire form used in the JSON Schemas.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "private" => Some(Self::Private),
            "inherit" => Some(Self::Inherit),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }

    /// Wire form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }
}

/// Transport scheme of a single upstream endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EndpointScheme {
    /// Plaintext HTTP. Legal when `allow_http_upstream` is enabled.
    #[serde(rename = "http")]
    Http,
    /// HTTP over TLS.
    #[default]
    #[serde(rename = "https")]
    Https,
    /// WebSocket over TLS.
    #[serde(rename = "wss")]
    Wss,
    /// WebTransport.
    #[serde(rename = "wt")]
    Wt,
    /// gRPC over TLS.
    #[serde(rename = "grpc")]
    Grpc,
}

impl EndpointScheme {
    /// Wire form.
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

    /// Parse the wire form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "wss" => Some(Self::Wss),
            "wt" => Some(Self::Wt),
            "grpc" => Some(Self::Grpc),
            _ => None,
        }
    }

    /// Whether the scheme is encrypted in transit.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }

    /// Whether the scheme speaks HTTP on the wire (all of them here, since gRPC
    /// is HTTP/2).
    #[must_use]
    pub fn is_http_family(self) -> bool {
        matches!(self, Self::Http | Self::Https)
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Transport scheme. Defaults to `https`.
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// Hostname or IP address of the upstream service.
    pub host: String,
    /// TCP port. Defaults to 443.
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Endpoint {
    /// `host[:port]` as used in error messages and target-host matching.
    #[must_use]
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Port used when the endpoint omits one.
const fn default_port() -> u16 {
    443
}

/// Endpoint pool of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// At least one endpoint.
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin binding on an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode for hierarchical override.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Which inbound headers are forwarded upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PassthroughMode {
    /// Forward none (default).
    #[default]
    #[serde(rename = "none")]
    None,
    /// Forward only names in `passthrough_allowlist`.
    #[serde(rename = "allowlist")]
    Allowlist,
    /// Forward all.
    #[serde(rename = "all")]
    All,
}

impl PassthroughMode {
    /// Parse the wire form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "none" => Some(Self::None),
            "allowlist" => Some(Self::Allowlist),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Ordered header mutation on a single header-name/value map.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeaderSetRules {
    /// Overwrite if present.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append (duplicates allowed).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Remove by name.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Outbound request header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeaderRequestRules {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward. Defaults to none.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Names forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeaderResponseRules {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation configuration on an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: HeaderRequestRules,
    #[serde(default)]
    pub response: HeaderResponseRules,
}

/// Token-bucket window unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RateWindow {
    #[default]
    #[serde(rename = "second")]
    Second,
    #[serde(rename = "minute")]
    Minute,
    #[serde(rename = "hour")]
    Hour,
    #[serde(rename = "day")]
    Day,
}

impl RateWindow {
    /// Window length in seconds.
    #[must_use]
    pub fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RateAlgorithm {
    #[default]
    #[serde(rename = "token_bucket")]
    TokenBucket,
    #[serde(rename = "sliding_window")]
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, Hash)]
pub enum RateScope {
    #[serde(rename = "global")]
    Global,
    #[default]
    #[serde(rename = "tenant")]
    Tenant,
    #[serde(rename = "user")]
    User,
    #[serde(rename = "ip")]
    Ip,
    #[serde(rename = "route")]
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RateStrategy {
    #[default]
    #[serde(rename = "reject")]
    Reject,
    #[serde(rename = "queue")]
    Queue,
    #[serde(rename = "degrade")]
    Degrade,
}

/// Sustained replenishment rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window. Minimum 1.
    pub rate: u64,
    /// Window unit. Defaults to `second`.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstConfig {
    /// Maximum bucket size.
    pub capacity: u64,
}

/// Token-bucket rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate; required when a rate limit is present.
    pub sustained: Option<SustainedRate>,
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    /// Counter scope. Defaults to `tenant`.
    #[serde(default)]
    pub scope: RateScope,
    /// Strategy. Defaults to `reject`.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request. Minimum 1, default 1.
    #[serde(default = "default_cost")]
    pub cost: u64,
    /// Emit `X-RateLimit-*` headers. Defaults to true.
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

/// Booleans default to true in the schemas.
const fn default_true() -> bool {
    true
}

/// The schema default of `rate_limit.cost`.
const fn default_cost() -> u64 {
    1
}

impl Default for RateLimitConfig {
    /// The schema defaults: `response_headers` is opt-out, everything else is
    /// the most permissive value.
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: None,
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
            response_headers: true,
        }
    }
}

/// Public form of [`default_true`] for DTOs reusing the same convention.
#[must_use]
pub const fn default_enabled() -> bool {
    true
}

/// CORS configuration for an upstream or route.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS for this upstream/route. Defaults to false.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Defaults to `GET, POST`.
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

/// A plugin bound to an upstream or route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginBinding {
    /// Canonical plugin identifier (GTS id or custom plugin UUID).
    pub plugin_ref: String,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Ordered plugin chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Inbound matching rules for a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MatchConfig {
    /// HTTP match; present when the upstream protocol is HTTP.
    #[serde(default)]
    pub http: Option<HttpMatch>,
    /// gRPC match; present when the upstream protocol is gRPC.
    #[serde(default)]
    pub grpc: Option<GrpcMatch>,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMatch {
    /// Method allowlist. At least one.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path pattern the request path must start with.
    pub path: String,
    /// Allowed query parameters. Empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How the proxy URL path suffix is treated. Defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How the proxy path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PathSuffixMode {
    /// Reject requests that carry a suffix beyond the route path.
    #[serde(rename = "disabled")]
    Disabled,
    /// Append the suffix to the route path (default).
    #[default]
    #[serde(rename = "append")]
    Append,
}

/// gRPC match rules (catalogued; not proxied in this phase).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Normalised HTTP method used for matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
    Options,
}

impl HttpMethod {
    /// Parse an HTTP method name, upper-cased.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            _ => None,
        }
    }

    /// Canonical upper-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

/// Tenant-scoped upstream configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing identifier; unique per tenant.
    pub alias: String,
    /// Protocol identifier (`gts.cf.core.oagw.protocol.v1~...`).
    pub protocol: String,
    /// Whether the upstream accepts traffic. Defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Optional auth plugin binding.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Optional header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Optional rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Optional CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Optional plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Upstream {
    /// The rate limit descendants may not override.
    ///
    /// A `share.enforce` rate limit on a parent upstream always participates in
    /// the hierarchical merge, even when the child configures its own limit.
    #[must_use]
    pub fn enforced_rate_limit(&self) -> Option<&RateLimitConfig> {
        self.rate_limit
            .as_ref()
            .filter(|limit| matches!(limit.sharing, SharingMode::Enforce))
    }
}

/// The CORS configuration in force for a proxied request.
///
/// A route-level configuration replaces the upstream's; otherwise the
/// upstream's applies unchanged.
#[must_use]
pub fn effective_cors<'a>(
    upstream: Option<&'a CorsConfig>,
    route: Option<&'a CorsConfig>,
) -> Option<&'a CorsConfig> {
    route.or(upstream)
}

/// Tenant-scoped route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream. Immutable after creation.
    pub upstream_id: Uuid,
    /// Protocol-scoped match rule.
    #[serde(rename = "match")]
    pub match_rule: MatchConfig,
    /// Match priority; higher wins before prefix length is considered.
    #[serde(default)]
    pub priority: i64,
    /// Whether the route participates in matching. Defaults to true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional rate limit override.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Optional CORS override.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Optional plugin chain appended after the upstream's.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Plugin kind, derived from the GTS base type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    /// Parse the wire form.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }

    /// The GTS base type for this kind.
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            Self::Auth => gts::AUTH_PLUGIN_TYPE,
            Self::Guard => gts::GUARD_PLUGIN_TYPE,
            Self::Transform => gts::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Lower-case name used on the wire.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}

/// Tenant-defined plugin (Starlark source is stored verbatim).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin kind (`auth`, `guard` or `transform`).
    #[serde(rename = "plugin_type")]
    pub kind: PluginKind,
    /// Tenant-unique name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional JSON Schema the plugin config must satisfy.
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    /// Plugin source text.
    #[serde(default)]
    pub source_code: Option<String>,
    /// Phases the plugin supports.
    #[serde(default)]
    pub phases: Vec<String>,
}

/// A resource that may carry `created_at`/`updated_at` bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Timestamps {
    /// Milliseconds since the Unix epoch, assigned on create.
    #[serde(default)]
    pub created_at_ms: u64,
    /// Milliseconds since the Unix epoch, refreshed on replace.
    #[serde(default)]
    pub updated_at_ms: u64,
}
