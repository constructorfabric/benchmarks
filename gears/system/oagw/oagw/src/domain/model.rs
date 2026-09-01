//! Domain model (DESIGN §3.1).
//!
//! The types here are both the internal model and the wire representation of
//! the management API — the JSON shapes mirror `schemas/upstream.v1.schema.json`
//! and `schemas/route.v1.schema.json`. Timestamps and tenant identity are
//! internal-only (`serde(skip)`) because the published schema does not carry
//! them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// GTS base-type identifiers for OAGW resources (DESIGN §3.1).
pub mod gts {
    use uuid::Uuid;

    /// Upstream base type.
    pub const UPSTREAM: &str = "gts.cf.core.oagw.upstream.v1~";
    /// Route base type.
    pub const ROUTE: &str = "gts.cf.core.oagw.route.v1~";
    /// Auth plugin base type.
    pub const AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~";
    /// Guard plugin base type.
    pub const GUARD_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~";
    /// Transform plugin base type.
    pub const TRANSFORM_PLUGIN: &str = "gts.cf.core.oagw.transform_plugin.v1~";
    /// Built-in pass-through authentication plugin.
    pub const NOOP_AUTH_PLUGIN: &str = "cf.core.oagw.noop.v1";
    /// HTTP protocol identifier.
    pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    /// gRPC protocol identifier.
    pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

    /// Anonymous GTS instance identifier for a resource.
    #[must_use]
    pub fn instance(base: &str, uuid: &Uuid) -> String {
        format!("{base}{uuid}")
    }
}

/// Sharing mode for hierarchical configuration fields (DESIGN §3.2).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants cannot override; the ancestor value wins.
    Enforce,
}

impl Sharing {
    /// Parse the wire spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "private" => Some(Self::Private),
            "inherit" => Some(Self::Inherit),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }

    /// Wire spelling.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }
}

/// Endpoint transport scheme.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// HTTPS (TLS).
    Https,
    /// Secure WebSocket (TLS).
    Wss,
    /// WebTransport (TLS).
    Wt,
    /// gRPC over TLS.
    Grpc,
    /// Plaintext HTTP — accepted only when `allow_http_upstream` is enabled.
    Http,
    /// Plaintext WebSocket — accepted only when `allow_http_upstream` is enabled.
    Ws,
}

impl Scheme {
    /// Parse the wire spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "https" => Some(Self::Https),
            "wss" => Some(Self::Wss),
            "wt" => Some(Self::Wt),
            "grpc" => Some(Self::Grpc),
            "http" => Some(Self::Http),
            "ws" => Some(Self::Ws),
            _ => None,
        }
    }

    /// Wire spelling.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
            Self::Http => "http",
            Self::Ws => "ws",
        }
    }

    /// `true` for TLS-bearing schemes.
    #[must_use]
    pub const fn is_tls(&self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Wt | Self::Grpc)
    }

    /// `true` for plaintext schemes gated by `allow_http_upstream`.
    #[must_use]
    pub const fn is_plaintext(&self) -> bool {
        matches!(self, Self::Http | Self::Ws)
    }

    /// Port that is omitted from a derived alias for this scheme.
    #[must_use]
    pub const fn standard_port(&self) -> u16 {
        match self {
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
            Self::Http | Self::Ws => 80,
        }
    }

    /// `true` when the scheme carries WebSocket traffic.
    #[must_use]
    pub const fn is_websocket(&self) -> bool {
        matches!(self, Self::Ws | Self::Wss)
    }
}

/// A single upstream endpoint (DESIGN §3.1 `Endpoint`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct Endpoint {
    /// Transport scheme.
    pub scheme: Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; defaults to the scheme's standard port.
    pub port: u16,
}

impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct Wire {
            scheme: Scheme,
            host: String,
            #[serde(default)]
            port: Option<u16>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            scheme: wire.scheme,
            host: wire.host,
            port: wire.port.unwrap_or_else(|| wire.scheme.standard_port()),
        })
    }
}

impl Endpoint {
    /// `host:port` used for dialling.
    #[must_use]
    pub fn socket_target(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// `true` when the endpoint targets an IP literal rather than a hostname.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }
}

/// Server configuration: one or more endpoints forming a pool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct ServerConfig {
    /// Endpoints (at least one).
    pub endpoints: Vec<Endpoint>,
}

/// Upstream protocol selector (DESIGN §3.3 request classification).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub enum Protocol {
    /// HTTP/1.1 and HTTP/2 proxying (route match by method + path prefix).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC (Phase 3: catalogued but no proxy code path is reachable).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// Wire spelling.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Http => gts::PROTOCOL_HTTP,
            Self::Grpc => gts::PROTOCOL_GRPC,
        }
    }
}

/// Authentication plugin configuration for an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct AuthConfig {
    /// Authentication plugin GTS identifier.
    ///
    /// Published as `auth.type` (`upstream.v1.schema.json`); the legacy
    /// `plugin_type` spelling is accepted on input.
    #[serde(
        default,
        rename = "type",
        alias = "plugin_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub plugin_type: Option<String>,
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            plugin_type: Some(gts::AUTH_PLUGIN.to_owned() + gts::NOOP_AUTH_PLUGIN),
            sharing: Sharing::Private,
            config: serde_json::Value::Null,
        }
    }
}

impl AuthConfig {
    /// `true` when the level declares no authentication of its own.
    ///
    /// Deserialising a level without an `auth` block yields the noop default,
    /// which is indistinguishable from an explicit `noop` choice only through
    /// the sharing mode: an unspecified level always keeps the `private`
    /// default and carries no plugin configuration.
    #[must_use]
    pub fn is_unspecified(&self) -> bool {
        self.plugin_type.is_none()
            || (self.sharing == Sharing::Private
                && self.config.is_null()
                && self.plugin_type.as_deref()
                    == Some(gts::AUTH_PLUGIN.to_owned() + gts::NOOP_AUTH_PLUGIN).as_deref())
    }

    /// UUID backing the referenced plugin, when it is a custom plugin.
    ///
    /// Built-in plugins name a catalogued GTS id whose instance part is not a
    /// UUID, so they yield `None` (DESIGN §3.6 "Plugin binding positions").
    #[must_use]
    pub fn plugin_uuid(&self) -> Option<Uuid> {
        self.plugin_type
            .as_deref()
            .and_then(|reference| reference.rsplit('~').next())
            .and_then(|part| Uuid::parse_str(part).ok())
    }
}

/// Inbound header passthrough policy (DESIGN §3.2 Headers Transformation).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward no inbound headers (default).
    #[default]
    None,
    /// Forward only the configured allowlist.
    Allowlist,
    /// Forward everything except routing and hop-by-hop headers.
    All,
}

/// Request header transformation rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct RequestHeaders {
    /// Headers to set (overwrite).
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    pub passthrough: Passthrough,
    /// Allowlist used when `passthrough` is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct ResponseHeaders {
    /// Headers to set on the client response (overwrite).
    pub set: BTreeMap<String, String>,
    /// Headers to add to the client response.
    pub add: BTreeMap<String, String>,
    /// Header names stripped from the upstream response.
    pub remove: Vec<String>,
}

/// Header transformation rules for an upstream.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct HeadersConfig {
    /// Request-side rules.
    pub request: RequestHeaders,
    /// Response-side rules.
    pub response: ResponseHeaders,
}

/// Rate-limit window unit (ADR-0003).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Window {
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

impl Window {
    /// Window length in seconds.
    #[must_use]
    pub const fn seconds(&self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Rate-limit counter scope (ADR-0003).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exhausted (ADR-0003).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429 (default).
    #[default]
    Reject,
    /// Queue the request (treated as reject in the MVP).
    Queue,
    /// Degrade (treated as reject in the MVP).
    Degrade,
}

/// Rate-limit algorithm (ADR-0003).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket with burst capacity (default).
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Sustained rate definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window unit.
    #[serde(default)]
    pub window: Window,
}

/// Burst configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub struct BurstConfig {
    /// Bucket capacity; defaults to the sustained rate.
    #[serde(default)]
    pub capacity: Option<u64>,
}

/// Rate-limit configuration (ADR-0003 dual-rate model).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Algorithm.
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity.
    pub burst: BurstConfig,
    /// Counter scope.
    pub scope: RateScope,
    /// Behaviour on exhaustion.
    pub strategy: RateStrategy,
    /// Cost charged per request.
    pub cost: u64,
    /// Whether `X-RateLimit-*` headers are emitted.
    pub response_headers: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 100,
                window: Window::Second,
            },
            burst: BurstConfig { capacity: None },
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }
}

impl RateLimitConfig {
    /// Effective bucket capacity (burst capacity or sustained rate).
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst.capacity.unwrap_or(self.sustained.rate)
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Whether CORS handling is active for this resource.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    pub allowed_origins: Vec<String>,
    /// Allowed methods for cross-origin requests.
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the safelist.
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed (incompatible with `["*"]`).
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// A plugin binding on an upstream or route.
#[derive(Debug, Clone, Serialize, PartialEq, utoipa::ToSchema)]
pub struct PluginBinding {
    /// Canonical plugin identifier (GTS id for named plugins, `uuid` string
    /// for custom plugins).
    pub plugin_ref: String,
    /// Extracted UUID when the reference is UUID-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Wire shape: either a bare identifier string (the published schemas'
        /// `items` type) or the full object with per-binding configuration.
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Wire {
            /// `"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"`
            Reference(String),
            /// `{"plugin_ref": "...", "config": {...}}`.
            Detailed {
                /// Canonical plugin identifier.
                plugin_ref: String,
                /// Extracted UUID when the reference is UUID-backed.
                #[serde(default, skip_serializing_if = "Option::is_none")]
                plugin_uuid: Option<Uuid>,
                /// Plugin-specific configuration.
                #[serde(default)]
                config: serde_json::Value,
            },
        }
        let reference = match Wire::deserialize(deserializer)? {
            Wire::Reference(reference) => PluginBinding::from_ref(reference),
            Wire::Detailed {
                plugin_ref,
                plugin_uuid,
                config,
            } => {
                let extracted = plugin_ref
                    .rsplit('~')
                    .next()
                    .and_then(|part| Uuid::parse_str(part).ok());
                PluginBinding {
                    plugin_ref,
                    plugin_uuid: plugin_uuid.or(extracted),
                    config,
                }
            }
        };
        Ok(reference)
    }
}

impl PluginBinding {
    /// Build a binding from a reference string.
    #[must_use]
    pub fn from_ref(plugin_ref: impl Into<String>) -> Self {
        let plugin_ref = plugin_ref.into();
        let plugin_uuid = plugin_ref
            .rsplit('~')
            .next()
            .and_then(|part| Uuid::parse_str(part).ok());
        Self {
            plugin_ref,
            plugin_uuid,
            config: serde_json::Value::Null,
        }
    }
}

/// Ordered plugin chain with a sharing mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct PluginsConfig {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Ordered bindings; positions are contiguous from zero.
    pub items: Vec<PluginBinding>,
}

/// Path-suffix handling for HTTP matches.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests that carry a path suffix.
    Disabled,
    /// Append the suffix to the matched path (default).
    #[default]
    Append,
}

/// HTTP request method allowed by a route match.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// GET
    Get,
    /// POST
    Post,
    /// PUT
    Put,
    /// DELETE
    Delete,
    /// PATCH
    Patch,
}

impl HttpMethod {
    /// Wire spelling.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }

    /// Parse from an HTTP method token.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }
}

/// HTTP match rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HttpMatch {
    /// Allowed methods (at least one).
    pub methods: Vec<HttpMethod>,
    /// Path pattern (longest-prefix match key).
    pub path: String,
    /// Allowed query parameters; empty allows none.
    pub query_allowlist: Vec<String>,
    /// Path-suffix handling.
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: vec![HttpMethod::Get],
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// gRPC match rules (Phase 3: catalogued, not reachable through the proxy).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules; exactly one variant is present.
///
/// Externally tagged so the wire shape is `{"http": {...}}` / `{"grpc": {...}}`
/// as published in `route.v1.schema.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
pub enum MatchConfig {
    /// HTTP match keys.
    #[serde(rename = "http")]
    Http(HttpMatch),
    /// gRPC match keys (Phase 3: catalogued, not reachable through the proxy).
    #[serde(rename = "grpc")]
    Grpc(GrpcMatch),
}

/// Route-level CORS configuration (DESIGN §3.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(default)]
pub struct RouteCorsConfig {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Whether CORS handling is active for this route.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    pub allowed_origins: Vec<String>,
    /// Allowed methods for cross-origin requests.
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the safelist.
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    pub allow_credentials: bool,
}

impl Default for RouteCorsConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl From<&RouteCorsConfig> for CorsConfig {
    fn from(value: &RouteCorsConfig) -> Self {
        Self {
            sharing: value.sharing,
            enabled: value.enabled,
            allowed_origins: value.allowed_origins.clone(),
            allowed_methods: value.allowed_methods.clone(),
            expose_headers: value.expose_headers.clone(),
            allow_credentials: value.allow_credentials,
        }
    }
}

/// Upstream entity (DESIGN §3.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Upstream {
    /// System-generated identifier (absent on create).
    #[serde(default)]
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Routing identifier; derived when omitted on a derivable endpoint pool.
    #[serde(default)]
    pub alias: String,
    /// Protocol used to connect.
    pub protocol: Protocol,
    /// Whether the upstream accepts traffic.
    #[serde(default = "crate::domain::validation::default_true")]
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeadersConfig,
    /// Rate-limit configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation instant (epoch milliseconds).
    #[serde(skip)]
    pub created_at: u64,
    /// Last modification instant (epoch milliseconds).
    #[serde(skip)]
    pub updated_at: u64,
}

/// Route entity (DESIGN §3.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Route {
    /// System-generated identifier (absent on create).
    #[serde(default)]
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Owning upstream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<Uuid>,
    /// Match rules.
    pub r#match: MatchConfig,
    /// Ordering priority for equal-length prefixes.
    #[serde(default)]
    pub priority: i64,
    /// Whether the route accepts traffic.
    #[serde(default = "crate::domain::validation::default_true")]
    pub enabled: bool,
    /// Route-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<RouteCorsConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Creation instant (epoch milliseconds).
    #[serde(skip)]
    pub created_at: u64,
    /// Last modification instant (epoch milliseconds).
    #[serde(skip)]
    pub updated_at: u64,
}

/// Custom (Starlark) plugin entity (DESIGN §3.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct Plugin {
    /// System-generated identifier (UUID part of the GTS id, absent on create).
    #[serde(default)]
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Plugin type: `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Human-readable name; unique per tenant.
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON schema describing the accepted plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source (custom plugins only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Last use instant (epoch milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    /// GC eligibility instant (epoch milliseconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<u64>,
}

impl Upstream {
    /// The upstream's canonical GTS identifier.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts::instance(gts::UPSTREAM, &self.id)
    }
}

impl Route {
    /// The route's canonical GTS identifier.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts::instance(gts::ROUTE, &self.id)
    }

    /// The HTTP path prefix when this route matches by HTTP rules.
    #[must_use]
    pub fn match_path(&self) -> &str {
        match &self.r#match {
            MatchConfig::Http(match_rules) => match_rules.path.as_str(),
            MatchConfig::Grpc(_) => "",
        }
    }
}

impl Plugin {
    /// The GTS base type for this plugin's type category.
    #[must_use]
    pub fn base_type(&self) -> &'static str {
        match self.plugin_type.as_str() {
            "auth" => gts::AUTH_PLUGIN,
            "transform" => gts::TRANSFORM_PLUGIN,
            _ => gts::GUARD_PLUGIN,
        }
    }

    /// The plugin's canonical GTS identifier.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts::instance(self.base_type(), &self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_round_trips_with_defaults() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "api.openai.com".to_owned(),
            protocol: Protocol::Http,
            enabled: true,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: vec!["llm".to_owned()],
            created_at: 1,
            updated_at: 1,
        };
        let json = serde_json::to_value(&upstream).expect("serializes");
        assert_eq!(json["alias"], "api.openai.com");
        assert_eq!(json["enabled"], true);
        assert!(json.get("tenant_id").is_none());
        let mut back: Upstream = serde_json::from_value(json).expect("deserializes");
        assert!(back.enabled);
        // Internal-only fields are `serde(skip)`: the wire round trip preserves
        // everything the schema publishes and the store re-stamps the rest.
        back.tenant_id = upstream.tenant_id;
        back.created_at = upstream.created_at;
        back.updated_at = upstream.updated_at;
        assert_eq!(back, upstream);
    }

    #[test]
    fn protocol_round_trips_through_the_wire_spelling() {
        let raw = serde_json::json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
        let parsed: Protocol = serde_json::from_value(raw).expect("parses");
        assert_eq!(parsed, Protocol::Http);
        assert_eq!(
            serde_json::to_value(Protocol::Grpc).expect("serializes"),
            serde_json::json!(gts::PROTOCOL_GRPC)
        );
    }

    #[test]
    fn match_config_accepts_http_and_grpc_shapes() {
        // The published route schema wraps the match keys in a protocol tag.
        let http: MatchConfig = serde_json::from_value(serde_json::json!({
            "http": {"methods": ["GET"], "path": "/v1"}
        }))
        .expect("parses");
        assert!(matches!(http, MatchConfig::Http(_)));
        assert_eq!(
            serde_json::to_value(&http).expect("serializes")["http"]["path"],
            "/v1"
        );
        let grpc: MatchConfig = serde_json::from_value(serde_json::json!({
            "grpc": {"service": "foo.v1.UserService", "method": "GetUser"}
        }))
        .expect("parses");
        assert!(matches!(grpc, MatchConfig::Grpc(_)));
    }

    #[test]
    fn plugin_bindings_accept_the_published_shapes() {
        let id = Uuid::new_v4();
        let reference = gts::instance(gts::GUARD_PLUGIN, &id);
        let from_string: PluginBinding =
            serde_json::from_value(serde_json::json!(reference)).expect("string form");
        assert_eq!(from_string.plugin_ref, reference);
        assert_eq!(from_string.plugin_uuid, Some(id));
        let from_object: PluginBinding = serde_json::from_value(serde_json::json!({
            "plugin_ref": reference,
            "config": {"header": "x-required"}
        }))
        .expect("object form");
        assert_eq!(from_object.config["header"], "x-required");
        assert_eq!(from_object.plugin_uuid, Some(id));
        assert_eq!(
            serde_json::to_value(&from_string).expect("serializes")["plugin_ref"],
            reference
        );
    }

    #[test]
    fn plugin_binding_extracts_uuid_backing() {
        let id = Uuid::new_v4();
        let binding = PluginBinding::from_ref(gts::instance(gts::GUARD_PLUGIN, &id));
        assert_eq!(binding.plugin_uuid, Some(id));
        let named = PluginBinding::from_ref(format!(
            "{}cf.core.oagw.required_headers.v1",
            gts::GUARD_PLUGIN
        ));
        assert_eq!(named.plugin_uuid, None);
    }

    #[test]
    fn plugin_gts_id_uses_type_category() {
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            plugin_type: "guard".to_owned(),
            name: "validator".to_owned(),
            description: None,
            config_schema: None,
            source_code: Some("def on_request(ctx): pass".to_owned()),
            last_used_at: None,
            gc_eligible_at: None,
        };
        assert!(plugin.gts_id().starts_with(gts::GUARD_PLUGIN));
    }
}
