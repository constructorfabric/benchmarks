//! Domain resource model.
//!
//! The wire shapes mirror `docs/schemas/{upstream,route}.v1.schema.json`
//! field-for-field. `deny_unknown_fields` enforces the schemas'
//! `additionalProperties: false`, which is what `FR-002` requires of a
//! submission. `enabled` and `priority` are declared on [`Route`] because
//! `DESIGN.md` § 3.1 models them there even though the published schema omits
//! them; declaring them is strictly more permissive than rejecting them.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Upstream protocol: HTTP.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Upstream protocol: gRPC.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Hierarchical sharing mode.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; a descendant may override.
    Inherit,
    /// Visible; a descendant may not override.
    Enforce,
}

impl SharingMode {
    /// Lower-case wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }
}

/// Upstream endpoint scheme.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP — accepted as input; dialling it is gated by
    /// `OagwConfig::allow_http_upstream` (`research.md` R4).
    Http,
    /// HTTPS (default).
    #[default]
    Https,
    /// Secure WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over HTTP/2.
    Grpc,
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Scheme {
    /// Lowercase wire name of the scheme.
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

    /// Whether the scheme secures the connection with TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }
}

/// Upstream protocol.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Protocol {
    /// HTTP / HTTP-1.1 and SSE.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    #[default]
    Http,
    /// gRPC over HTTP/2.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// The GTS identifier of the protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }
}

/// Declared endpoint of an upstream's pool.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Endpoint {
    /// Endpoint scheme.
    pub scheme: Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; defaults to 443.
    pub port: Option<u16>,
}

impl Endpoint {
    /// The effective port, defaulting to 443.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(443)
    }

    /// `host:port` as used for the outbound authority.
    #[must_use]
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port())
    }
}

/// `server` block.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoint pool; at least one entry.
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin binding.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AuthConfig {
    /// Authentication plugin identifier (GTS id of an auth plugin instance).
    #[serde(rename = "type")]
    pub kind: String,
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Plugin configuration, including optional `secret_ref` for credentials.
    pub config: serde_json::Value,
}

/// Request-header rules.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to set, overwriting any existing value.
    pub set: BTreeMap<String, String>,
    /// Headers to add, allowing duplicates.
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    pub passthrough: PassthroughMode,
    /// Headers to forward when `passthrough` is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// Which inbound headers are forwarded to the upstream.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward none of the inbound request headers (default).
    #[default]
    None,
    /// Forward only the allow-listed names.
    Allowlist,
    /// Forward everything except hop-by-hop and gateway-reserved headers.
    All,
}

/// Response-header rules.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    pub add: BTreeMap<String, String>,
    /// Headers to strip from the upstream response.
    pub remove: Vec<String>,
}

/// Header transformation rules for both legs.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HeaderRules {
    /// Applied to the forwarded request.
    pub request: RequestHeaderRules,
    /// Applied to the response returned to the client.
    pub response: ResponseHeaderRules,
}

/// One entry of a plugin binding list.
///
/// The published schema models `items[]` as a bare reference — a built-in GTS
/// id or a custom plugin UUID — while `ADR/0009` § "Upstream Configuration
/// Example" shows the object form, which is how a binding carries its own
/// configuration. Both are accepted; a binding without a `config` behaves
/// exactly like the bare reference.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBinding {
    /// Bare reference: built-in GTS id or custom plugin UUID.
    Ref(String),
    /// Reference with an explicit per-binding configuration.
    Configured {
        /// Built-in GTS id or custom plugin UUID.
        #[serde(alias = "type")]
        plugin_ref: String,
        /// Configuration handed to the plugin as `ctx.config`.
        #[serde(default)]
        config: serde_json::Value,
    },
}

/// Configuration used when a binding carries none.
const NO_CONFIG: serde_json::Value = serde_json::Value::Null;

impl PluginBinding {
    /// The referenced plugin identifier.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            Self::Ref(reference) => reference,
            Self::Configured { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The binding's configuration; `Null` for the bare reference form.
    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        match self {
            Self::Ref(_) => &NO_CONFIG,
            Self::Configured { config, .. } => config,
        }
    }
}

/// Plugin binding list.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PluginSet {
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Bindings: built-in GTS ids, custom plugin UUIDs, or either with a
    /// per-binding configuration.
    pub items: Vec<PluginBinding>,
}

impl PluginSet {
    /// The referenced identifiers, in binding order.
    #[must_use]
    pub fn references(&self) -> Vec<&str> {
        self.items.iter().map(PluginBinding::reference).collect()
    }
}

/// Sustained rate.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Time window for the sustained rate.
    pub window: RateWindow,
}

/// Rate-limit window.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// One second (default).
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
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

/// Burst configuration.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BurstConfig {
    /// Bucket capacity; defaults to the sustained rate.
    pub capacity: Option<u32>,
}

/// Rate-limiting algorithm.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(
    utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One counter for everyone.
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

/// Behaviour when the limit is exceeded.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Serve a degraded response.
    Degrade,
}

/// Rate-limit configuration.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimit {
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Rate-limiting algorithm.
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity.
    pub burst: BurstConfig,
    /// Counter scope.
    pub scope: RateScope,
    /// Over-limit strategy.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u32,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::default(),
            },
            burst: BurstConfig::default(),
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
        }
    }
}

impl RateLimit {
    /// Effective bucket capacity, defaulting to the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate).max(1)
    }

    /// The sustained rate normalized to tokens per second.
    #[must_use]
    pub fn rate_per_second(&self) -> f64 {
        let window = self.sustained.window.seconds();
        f64::from(self.sustained.rate.max(1)) / f64::from(u32::try_from(window).unwrap_or(1))
    }
}

/// CORS configuration.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Cors {
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Whether CORS is enabled for this resource.
    pub enabled: bool,
    /// Allowed origins: `"*"` or absolute origin URIs.
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    pub allow_credentials: bool,
}

/// Route path-suffix handling.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject a request that carries a path suffix.
    Disabled,
    /// Append the path suffix to the route path (default).
    #[default]
    Append,
}

/// HTTP method accepted by a route.
#[derive(
    utoipa::ToSchema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Hash,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH`
    Patch,
}

impl HttpMethod {
    /// Uppercase wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// HTTP match rules.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct HttpMatch {
    /// Methods supported by this route; at least one.
    pub methods: Vec<HttpMethod>,
    /// Path prefix for the route.
    pub path: String,
    /// Query parameters the route accepts; empty allows none.
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated.
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rule; exactly one of `http` / `grpc` must be present.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MatchRule {
    /// HTTP match rules.
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    pub grpc: Option<GrpcMatch>,
}

/// A declared outbound service.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    /// Server-generated identifier (read-only).
    pub id: Option<Uuid>,
    /// Whether the upstream accepts proxy requests.
    pub enabled: bool,
    /// Routing alias, derived or explicit.
    pub alias: Option<String>,
    /// Flat tags; add-only across the hierarchy.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Authentication plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeaderRules>,
    /// Plugin binding list.
    pub plugins: Option<PluginSet>,
    /// Rate-limit configuration.
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    pub cors: Option<Cors>,
    /// Owning tenant (server-managed).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Creation timestamp, epoch milliseconds (server-managed).
    #[serde(skip)]
    pub created_at: u64,
    /// Last-update timestamp, epoch milliseconds (server-managed).
    #[serde(skip)]
    pub updated_at: u64,
}

/// A match rule binding a proxy request to an upstream.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Route {
    /// Server-generated identifier (read-only).
    pub id: Option<Uuid>,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Match priority; the longer matching prefix wins regardless.
    pub priority: i32,
    /// Owning upstream; immutable after creation.
    pub upstream_id: Option<Uuid>,
    /// Match rule.
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    /// Plugin binding list.
    pub plugins: Option<PluginSet>,
    /// Rate-limit configuration.
    pub rate_limit: Option<RateLimit>,
    /// Flat tags.
    pub tags: Vec<String>,
    /// Owning tenant (server-managed).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Creation timestamp, epoch milliseconds (server-managed).
    #[serde(skip)]
    pub created_at: u64,
    /// Last-update timestamp, epoch milliseconds (server-managed).
    #[serde(skip)]
    pub updated_at: u64,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: None,
            // A resource that omits `enabled` accepts proxy requests.
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: Protocol::default(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id: Uuid::nil(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl Upstream {
    /// `true` when the upstream is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: None,
            // A route that omits `enabled` participates in matching.
            enabled: true,
            priority: 0,
            upstream_id: None,
            match_rule: MatchRule::default(),
            plugins: None,
            rate_limit: None,
            tags: Vec::new(),
            tenant_id: Uuid::nil(),
            created_at: 0,
            updated_at: 0,
        }
    }
}

impl Route {
    /// `true` when the route participates in matching.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// A custom plugin definition; immutable once created.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Plugin {
    /// Server-generated identifier (read-only).
    pub id: Option<Uuid>,
    /// Plugin kind.
    #[serde(rename = "type")]
    pub kind: PluginKind,
    /// Unique name within `(tenant, kind)`.
    pub name: String,
    /// Starlark source; stored and served, never executed.
    pub source: String,
    /// Declared configuration schema.
    pub config_schema: serde_json::Value,
    /// Owning tenant (server-managed).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Creation timestamp, epoch milliseconds (server-managed).
    #[serde(skip)]
    pub created_at: u64,
}

/// Plugin kind.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Credential injection.
    #[default]
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request and response mutation.
    Transform,
}

impl PluginKind {
    /// Lower-case wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}
