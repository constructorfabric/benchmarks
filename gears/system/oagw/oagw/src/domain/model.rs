//! Domain model for upstreams, routes and plugins.
//!
//! The field names and value spaces here are the wire contract from
//! `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`; the types are shared between the
//! transport layer and the domain so there is exactly one definition of what
//! an upstream *is*.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::error::{OagwError, OagwResult};
use super::gts_helpers::{PROTOCOL_GRPC, PROTOCOL_HTTP};

/// Free-form plugin configuration (`ctx.config`).
///
/// A `BTreeMap` rather than a `serde_json::Map` so iteration order is
/// deterministic — the OAuth2 plugin hashes these pairs into a cache key.
pub type PluginConfig = BTreeMap<String, Value>;

/// Read a plugin-config key as a string, stringifying scalars so a JSON
/// number or boolean is as usable as a quoted value.
#[must_use]
pub fn config_str(config: &PluginConfig, key: &str) -> Option<String> {
    match config.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Sharing modes
// ---------------------------------------------------------------------------

/// Hierarchical visibility of a configuration block
/// (`cpt-cf-oagw-fr-hierarchical-config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; a descendant may override.
    Inherit,
    /// Visible; a descendant may not override.
    Enforce,
}

impl SharingMode {
    /// Whether descendants can see this block at all.
    #[must_use]
    pub fn visible_to_descendants(self) -> bool {
        matches!(self, Self::Inherit | Self::Enforce)
    }
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Endpoint scheme.
///
/// The TLS family (`https`/`wss`/`wt`/`grpc`) is what
/// `cpt-cf-oagw-constraint-https-only` describes as the default posture.
/// `http`/`ws` are accepted by the field so a deployment that lifts the
/// constraint via `allow_http_upstream` can express a plaintext upstream —
/// whether a plaintext connection is actually made is decided at connect
/// time, not at validation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// TLS HTTP.
    #[default]
    Https,
    /// TLS WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over TLS.
    Grpc,
    /// Plaintext HTTP.
    Http,
    /// Plaintext WebSocket.
    Ws,
}

impl Scheme {
    /// The port omitted from a derived alias.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether a connection with this scheme is wrapped in TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http | Self::Ws)
    }

    /// Wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
            Self::Http => "http",
            Self::Ws => "ws",
        }
    }
}

/// One member of an upstream's endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Connection scheme.
    pub scheme: Scheme,
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port; defaults to the scheme's standard port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// Whether `host` is an IP literal rather than a hostname.
    #[must_use]
    pub fn host_is_ip(&self) -> bool {
        crate::domain::alias::is_ip_literal(&self.host)
    }

    /// Normalised host: ASCII lowercase, trailing dot stripped.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        crate::domain::alias::normalize_host(&self.host)
    }
}

/// An upstream's endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// One or more endpoints forming a load-balance pool.
    pub endpoints: Vec<Endpoint>,
}

// ---------------------------------------------------------------------------
// Protocol
// ---------------------------------------------------------------------------

/// Upstream wire protocol, named by its GTS identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, utoipa::ToSchema)]
pub enum Protocol {
    /// HTTP (and everything layered on it: SSE, WebSocket).
    #[default]
    Http,
    /// gRPC. Catalogued; the proxy code path is phase 3.
    Grpc,
}

impl Protocol {
    /// GTS identifier for this protocol.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw == PROTOCOL_HTTP {
            Ok(Self::Http)
        } else if raw == PROTOCOL_GRPC {
            Ok(Self::Grpc)
        } else {
            Err(de::Error::custom(format!(
                "protocol must be one of [{PROTOCOL_HTTP}, {PROTOCOL_GRPC}], got {raw:?}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Upstream auth-plugin binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: PluginConfig,
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

/// Which inbound headers reach the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Passthrough {
    /// Forward nothing beyond representation metadata.
    None,
    /// Forward only the names in `passthrough_allowlist`.
    Allowlist,
    /// Forward everything that is not hop-by-hop or routing-internal.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeadersConfig {
    /// Headers to set (overwriting any inbound value).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Inbound header names to drop.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Passthrough policy. Absent means "transparent proxy": every
    /// non-hop-by-hop inbound header is forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<Passthrough>,
    /// Names forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

impl RequestHeadersConfig {
    /// Effective passthrough policy.
    #[must_use]
    pub fn effective_passthrough(&self) -> Passthrough {
        self.passthrough.unwrap_or(Passthrough::All)
    }
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeadersConfig {
    /// Headers to set on the response to the client.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append to the response.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Upstream response header names to strip.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeadersConfig>,
    /// Response-side rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeadersConfig>,
}

// ---------------------------------------------------------------------------
// Plugin chain
// ---------------------------------------------------------------------------

/// One entry in a plugin chain.
///
/// Accepted on the wire either as a bare identifier string (the form in the
/// JSON Schema) or as `{"plugin_ref": "...", "config": {...}}` (the form the
/// plugin ADRs use, and the only one that can carry configuration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct PluginBinding {
    /// Canonical plugin identifier.
    pub plugin_ref: String,
    /// Per-binding plugin configuration.
    #[serde(default, skip_serializing_if = "PluginConfig::is_empty")]
    #[schema(value_type = Object)]
    pub config: PluginConfig,
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Full {
            plugin_ref: String,
            #[serde(default)]
            config: PluginConfig,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Ref(String),
            Full(Full),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Ref(plugin_ref) => Ok(Self {
                plugin_ref,
                config: PluginConfig::new(),
            }),
            Wire::Full(Full { plugin_ref, config }) => Ok(Self { plugin_ref, config }),
        }
    }
}

/// A guard/transform plugin chain attached to an upstream or route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Hierarchical sharing mode for the chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered chain entries.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Rate-limit algorithm (`cpt-cf-oagw-adr-rate-limiting`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket — allows bursts up to `burst.capacity`.
    #[default]
    TokenBucket,
    /// Sliding window — no boundary burst.
    SlidingWindow,
}

/// Sustained-rate window unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    /// Per second.
    #[default]
    Second,
    /// Per minute.
    Minute,
    /// Per hour.
    Hour,
    /// Per day.
    Day,
}

impl RateWindow {
    /// Window length in seconds.
    #[must_use]
    pub fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Sustained {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window unit.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst allowance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    /// Bucket capacity. Defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

/// Hierarchical budget mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    /// No budget tracking.
    #[default]
    Unlimited,
    /// Parent allocates a fixed budget to children.
    Allocated,
    /// Children share the parent's budget.
    Shared,
}

/// Hierarchical budget allocation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Budget mode.
    #[serde(default)]
    pub mode: BudgetMode,
    /// Total budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Permitted overcommit ratio (1.0 – 2.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
}

/// Counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    /// One counter for the whole deployment.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    /// Reject with `429` and `Retry-After`.
    #[default]
    Reject,
    /// Wait for capacity within a bounded window.
    Queue,
    /// Serve with reduced functionality.
    Degrade,
}

fn default_cost() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

/// Rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate (required).
    pub sustained: Sustained,
    /// Burst allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    /// Hierarchical budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Overflow strategy.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Emit `X-RateLimit-*` response headers.
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

impl RateLimitConfig {
    /// Bucket capacity — `burst.capacity`, else `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst
            .and_then(|b| b.capacity)
            .unwrap_or(self.sustained.rate)
            .max(1)
    }

    /// Refill rate in tokens per second.
    #[must_use]
    pub fn tokens_per_second(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let rate = self.sustained.rate as f64;
        #[allow(clippy::cast_precision_loss)]
        let window = self.sustained.window.seconds() as f64;
        rate / window
    }

    /// Take the stricter of two limits, per
    /// `effective = min(ancestor.enforced, descendant)`.
    #[must_use]
    pub fn tighten(self, other: &Self) -> Self {
        if other.tokens_per_second() < self.tokens_per_second() {
            let mut chosen = other.clone();
            chosen.cost = self.cost.max(other.cost);
            chosen
        } else {
            let mut chosen = self;
            chosen.cost = chosen.cost.max(other.cost);
            chosen
        }
    }

    /// Reject configurations the schema forbids.
    ///
    /// # Errors
    ///
    /// `400` when `sustained.rate`, `burst.capacity`, `cost` or
    /// `budget.overcommit_ratio` fall outside their documented ranges.
    pub fn validate(&self, field: &str) -> OagwResult<()> {
        if self.sustained.rate == 0 {
            return Err(OagwError::field(
                field,
                "rate_limit.sustained.rate must be >= 1",
            ));
        }
        if self.burst.and_then(|b| b.capacity) == Some(0) {
            return Err(OagwError::field(
                field,
                "rate_limit.burst.capacity must be >= 1",
            ));
        }
        if self.cost == 0 {
            return Err(OagwError::field(field, "rate_limit.cost must be >= 1"));
        }
        if let Some(ratio) = self.budget.and_then(|b| b.overcommit_ratio)
            && !(1.0..=2.0).contains(&ratio)
        {
            return Err(OagwError::field(
                field,
                "rate_limit.budget.overcommit_ratio must be within [1.0, 2.0]",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// CORS configuration (`cpt-cf-oagw-adr-cors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is active. Disabled unless explicitly enabled.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_methods: Option<Vec<String>>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Effective allowed methods, defaulting to `["GET", "POST"]`.
    #[must_use]
    pub fn methods(&self) -> Vec<String> {
        self.allowed_methods
            .clone()
            .unwrap_or_else(|| vec!["GET".to_owned(), "POST".to_owned()])
    }

    /// Whether `origin` is permitted. Matching is exact — no patterns, and
    /// scheme/host/port all significant.
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }

    /// Whether the wildcard origin is configured.
    #[must_use]
    pub fn is_wildcard(&self) -> bool {
        self.allowed_origins.iter().any(|o| o == "*")
    }

    /// Reject the configuration the ADR calls out as unsafe.
    ///
    /// # Errors
    ///
    /// `400` when `allow_credentials` is combined with a wildcard origin.
    pub fn validate(&self, field: &str) -> OagwResult<()> {
        if self.allow_credentials && self.is_wildcard() {
            return Err(OagwError::field(
                field,
                "cors.allow_credentials cannot be combined with the wildcard origin '*'",
            ));
        }
        Ok(())
    }

    /// Merge a descendant's configuration over an ancestor's, per the
    /// sharing mode: `inherit` unions the origin lists, `enforce` keeps the
    /// ancestor's as-is.
    #[must_use]
    pub fn merge_descendant(ancestor: &Self, descendant: &Self) -> Self {
        if ancestor.sharing == SharingMode::Enforce {
            return ancestor.clone();
        }
        let mut merged = descendant.clone();
        for origin in &ancestor.allowed_origins {
            if !merged.allowed_origins.contains(origin) {
                merged.allowed_origins.push(origin.clone());
            }
        }
        merged
    }
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// A configured external service.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key used in proxy URLs.
    pub alias: String,
    /// Whether proxy requests are accepted.
    pub enabled: bool,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Auth plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Guard/transform chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limits.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Monotonic creation sequence, used for stable `$orderby`.
    pub seq: u64,
}

impl Upstream {
    /// Whether the pool needs `X-OAGW-Target-Host` to disambiguate
    /// (`cpt-cf-oagw-adr-request-routing` Appendix A).
    ///
    /// Only the *common-suffix* case is ambiguous: the alias names a domain
    /// rather than a host, so the gateway cannot tell which member of the
    /// pool the caller meant. A pool with an explicit alias (derivation
    /// failed — IP addresses, or hostnames with no registrable common
    /// suffix) is load-balanced instead, and a single-endpoint pool never
    /// needs the header.
    #[must_use]
    pub fn requires_target_host(&self) -> bool {
        if self.server.endpoints.len() < 2 {
            return false;
        }
        let Some(derived) = crate::domain::alias::compute_derived_alias(&self.server.endpoints)
        else {
            return false;
        };
        let derived_host = derived.split(':').next().unwrap_or(&derived);
        !self
            .server
            .endpoints
            .iter()
            .any(|endpoint| endpoint.normalized_host() == derived_host)
    }

    /// Endpoint hosts, in declaration order.
    #[must_use]
    pub fn endpoint_hosts(&self) -> Vec<String> {
        self.server
            .endpoints
            .iter()
            .map(Endpoint::normalized_host)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// How `/{path_suffix}` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject requests that carry a suffix beyond `match.http.path`.
    Disabled,
    /// Append the suffix to `match.http.path`.
    #[default]
    Append,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods this route accepts.
    pub methods: Vec<String>,
    /// Path prefix.
    pub path: String,
    /// Permitted query parameter names. Empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Path-suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules. Exactly one of `http` / `grpc` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP match keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A path on an upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Upstream this route belongs to.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Tie-breaker between routes with equally specific paths.
    pub priority: i32,
    /// Match rules.
    pub match_config: MatchConfig,
    /// Route-level rate limits.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS overrides.
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Monotonic creation sequence.
    pub seq: u64,
}

impl Route {
    /// The route's match type discriminator (`http` / `grpc`).
    #[must_use]
    pub fn match_type(&self) -> &'static str {
        if self.match_config.grpc.is_some() {
            "grpc"
        } else {
            "http"
        }
    }
}

// ---------------------------------------------------------------------------
// Custom plugin
// ---------------------------------------------------------------------------

/// A tenant-defined (Starlark) plugin, immutable after creation.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRecord {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin kind (`auth` / `guard` / `transform`).
    pub plugin_type: super::gts_helpers::PluginKind,
    /// Unique-per-tenant name.
    pub name: String,
    /// Free-text description.
    pub description: Option<String>,
    /// Declared phases (`on_request`, `on_response`, `on_error`).
    pub phases: Vec<String>,
    /// JSON Schema for this plugin's configuration.
    pub config_schema: Option<Value>,
    /// Starlark source.
    pub source_code: String,
    /// Instant (monotonic sequence) at which the plugin became unlinked.
    pub gc_eligible_at: Option<u64>,
    /// Monotonic creation sequence.
    pub seq: u64,
}

impl PluginRecord {
    /// Canonical GTS identifier for this plugin.
    #[must_use]
    pub fn plugin_ref(&self) -> String {
        super::gts_helpers::anonymous_id(self.plugin_type.base_type(), self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_default_ports_follow_the_design() {
        assert_eq!(Scheme::Http.default_port(), 80);
        assert_eq!(Scheme::Https.default_port(), 443);
        assert_eq!(Scheme::Wss.default_port(), 443);
        assert_eq!(Scheme::Grpc.default_port(), 443);
        assert!(!Scheme::Http.is_tls());
        assert!(Scheme::Wss.is_tls());
    }

    #[test]
    fn http_scheme_deserializes() {
        let endpoint: Endpoint =
            serde_json::from_value(serde_json::json!({"scheme": "http", "port": 80, "host": "x"}))
                .expect("plaintext scheme is a legal field value");
        assert_eq!(endpoint.scheme, Scheme::Http);
        assert_eq!(endpoint.port(), 80);
    }

    #[test]
    fn endpoint_port_defaults_to_the_scheme_port() {
        let endpoint: Endpoint = serde_json::from_value(
            serde_json::json!({"scheme": "https", "host": "api.openai.com"}),
        )
        .expect("port is optional");
        assert_eq!(endpoint.port(), 443);
    }

    #[test]
    fn unknown_endpoint_fields_are_rejected() {
        let err = serde_json::from_value::<Endpoint>(
            serde_json::json!({"scheme": "https", "host": "x", "nope": 1}),
        );
        assert!(err.is_err(), "additionalProperties: false");
    }

    #[test]
    fn protocol_round_trips_via_its_gts_id() {
        let protocol: Protocol = serde_json::from_value(serde_json::json!(PROTOCOL_HTTP))
            .expect("http protocol id parses");
        assert_eq!(protocol, Protocol::Http);
        assert_eq!(serde_json::to_value(protocol).unwrap(), PROTOCOL_HTTP);
        assert!(serde_json::from_value::<Protocol>(serde_json::json!("nope")).is_err());
    }

    #[test]
    fn plugin_bindings_accept_both_wire_forms() {
        let bare: PluginBinding = serde_json::from_value(serde_json::json!(
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        ))
        .expect("bare identifier form");
        assert!(bare.config.is_empty());

        let full: PluginBinding = serde_json::from_value(serde_json::json!({
            "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "config": {"required_request_headers": "x-correlation-id"}
        }))
        .expect("object form");
        assert_eq!(
            config_str(&full.config, "required_request_headers").as_deref(),
            Some("x-correlation-id")
        );
    }

    #[test]
    fn rate_limit_capacity_and_refill() {
        let cfg = RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained {
                rate: 120,
                window: RateWindow::Minute,
            },
            burst: Some(Burst { capacity: Some(10) }),
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        };
        assert_eq!(cfg.capacity(), 10);
        assert!((cfg.tokens_per_second() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn tighten_keeps_the_stricter_limit() {
        let loose = RateLimitConfig {
            sharing: SharingMode::Enforce,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained {
                rate: 10_000,
                window: RateWindow::Minute,
            },
            burst: None,
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        };
        let strict = RateLimitConfig {
            sustained: Sustained {
                rate: 100,
                window: RateWindow::Minute,
            },
            ..loose.clone()
        };
        assert_eq!(loose.clone().tighten(&strict).sustained.rate, 100);
        assert_eq!(strict.tighten(&loose).sustained.rate, 100);
    }

    #[test]
    fn cors_rejects_credentials_with_wildcard() {
        let cfg = CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: None,
            expose_headers: vec![],
            allow_credentials: true,
        };
        assert!(cfg.validate("cors").is_err());
    }

    #[test]
    fn cors_origin_matching_is_exact() {
        let cfg = CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: None,
            expose_headers: vec![],
            allow_credentials: false,
        };
        assert!(cfg.origin_allowed("https://app.example.com"));
        assert!(!cfg.origin_allowed("http://app.example.com"));
        assert!(!cfg.origin_allowed("https://app.example.com:8080"));
        assert!(!cfg.origin_allowed("https://evil.com"));
    }

    #[test]
    fn cors_inherit_unions_origins_and_enforce_does_not() {
        let ancestor = CorsConfig {
            sharing: SharingMode::Inherit,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: None,
            expose_headers: vec![],
            allow_credentials: false,
        };
        let descendant = CorsConfig {
            sharing: SharingMode::Private,
            allowed_origins: vec!["https://admin.example.com".to_owned()],
            ..ancestor.clone()
        };
        let merged = CorsConfig::merge_descendant(&ancestor, &descendant);
        assert_eq!(merged.allowed_origins.len(), 2);

        let enforced = CorsConfig {
            sharing: SharingMode::Enforce,
            ..ancestor
        };
        let merged = CorsConfig::merge_descendant(&enforced, &descendant);
        assert_eq!(merged.allowed_origins, vec!["https://app.example.com"]);
    }

    #[test]
    fn passthrough_defaults_to_transparent() {
        let cfg = RequestHeadersConfig::default();
        assert_eq!(cfg.effective_passthrough(), Passthrough::All);
        let explicit: RequestHeadersConfig =
            serde_json::from_value(serde_json::json!({"passthrough": "none"})).unwrap();
        assert_eq!(explicit.effective_passthrough(), Passthrough::None);
    }
}
