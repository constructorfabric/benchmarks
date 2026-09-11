//! Domain model, mirroring `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` (plus the `enabled` field PRD §5.1 adds
//! to both resources).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;

/// Sharing mode for hierarchical configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; descendants may override.
    Inherit,
    /// Visible; descendants may not override.
    Enforce,
}

/// Endpoint schemes. `http` is a legal value here — the *connection* policy for
/// plaintext upstreams is a separate gear-level flag (`allow_http_upstream`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum EndpointScheme {
    /// Plaintext HTTP. Legal whenever `allow_http_upstream` is `true`.
    #[serde(rename = "http")]
    Http,
    /// Default.
    #[default]
    #[serde(rename = "https")]
    Https,
    /// WebSocket over TLS.
    #[serde(rename = "wss")]
    Wss,
    /// WebTransport.
    #[serde(rename = "wt")]
    Wt,
    /// gRPC.
    #[serde(rename = "grpc")]
    Grpc,
}

impl EndpointScheme {
    /// Standard port for this scheme — omitted from derived aliases.
    #[must_use]
    pub fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

/// One upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Defaults to `https`.
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// RFC 1123 hostname or IP literal.
    pub host: String,
    /// Defaults to the scheme's standard port.
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port (the default for the scheme when unset).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.standard_port())
    }

    /// `true` when the port equals the scheme's standard port.
    #[must_use]
    pub fn has_standard_port(&self) -> bool {
        self.effective_port() == self.scheme.standard_port()
    }
}

/// `server` block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// One or more endpoints forming a load-balance pool.
    pub endpoints: Vec<Endpoint>,
}

/// `headers.request.passthrough` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound request headers (only structural ones).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header (minus routing + hop-by-hop).
    All,
}

/// `headers.request` rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RequestHeaderRules {
    /// Headers to set (overwriting).
    pub set: BTreeMap<String, String>,
    /// Headers to add (appending).
    pub add: BTreeMap<String, String>,
    /// Header names to remove.
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough` is `allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// `headers.response` rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ResponseHeaderRules {
    /// Headers to set on the client response.
    pub set: BTreeMap<String, String>,
    /// Headers to add to the client response.
    pub add: BTreeMap<String, String>,
    /// Headers stripped from the upstream response.
    pub remove: Vec<String>,
}

/// `headers` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HeadersConfig {
    /// Inbound → outbound rules.
    pub request: RequestHeaderRules,
    /// Upstream → client rules.
    pub response: ResponseHeaderRules,
}

/// `auth` block — which auth plugin injects outbound credentials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: Option<String>,
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Auth plugin configuration.
    pub config: BTreeMap<String, serde_json::Value>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            plugin_type: Some(gts::AUTH_NOOP.to_owned()),
            sharing: SharingMode::Private,
            config: BTreeMap::new(),
        }
    }
}

/// A `plugins.items[]` entry: either a bare plugin identifier or a bound
/// `{plugin_ref, config}` object (ADR 0009's configuration shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBinding {
    /// A bare GTS identifier or custom-plugin UUID.
    Reference(String),
    /// A plugin reference with optional bind-time configuration.
    Bound {
        /// Plugin identifier (GTS id or UUID).
        plugin_ref: String,
        /// UUID of the stored custom plugin, when the reference is UUID-backed.
        plugin_uuid: Option<Uuid>,
        /// Plugin configuration.
        config: Option<BTreeMap<String, serde_json::Value>>,
    },
}

impl PluginBinding {
    /// The referenced plugin identifier.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Reference(r) | Self::Bound { plugin_ref: r, .. } => r,
        }
    }

    /// The bound plugin's configuration, when supplied.
    #[must_use]
    pub fn config(&self) -> Option<&BTreeMap<String, serde_json::Value>> {
        match self {
            Self::Reference(_) => None,
            Self::Bound { config, .. } => config.as_ref(),
        }
    }
}

/// `plugins` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PluginsConfig {
    /// Sharing mode for the plugin chain.
    pub sharing: SharingMode,
    /// Plugins applied to this resource.
    pub items: Vec<PluginBinding>,
}

/// Rate-limit window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
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

impl RateWindow {
    /// Window length.
    ///
    /// Spelled in seconds: `Duration`'s minute/hour/day constructors are still
    /// unstable.
    #[must_use]
    #[allow(clippy::duration_suboptimal_units)]
    pub fn duration(self) -> std::time::Duration {
        match self {
            Self::Second => std::time::Duration::from_secs(1),
            Self::Minute => std::time::Duration::from_secs(60),
            Self::Hour => std::time::Duration::from_secs(60 * 60),
            Self::Day => std::time::Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// `rate_limit.sustained`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window for the sustained rate.
    #[serde(default)]
    pub window: RateWindow,
}

/// `rate_limit.burst`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Burst {
    /// Bucket capacity; defaults to the sustained rate.
    pub capacity: u32,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket for the whole deployment.
    Global,
    /// One bucket per tenant (default).
    #[default]
    Tenant,
    /// One bucket per authenticated user.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// `rate_limit` block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Sharing mode.
    pub sharing: SharingMode,
    /// Algorithm.
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Bucket capacity.
    pub burst: Option<Burst>,
    /// Counter scope.
    pub scope: RateScope,
    /// Overflow strategy.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u32,
    /// Whether to emit `X-RateLimit-*` headers on rejections.
    pub response_headers: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }
}

impl RateLimitConfig {
    /// Bucket capacity (defaults to the sustained rate).
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst
            .as_ref()
            .map_or(self.sustained.rate, |b| b.capacity)
    }
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Sharing mode.
    pub sharing: SharingMode,
    /// Enable CORS handling.
    pub enabled: bool,
    /// Allowed origins (`["*"]` allows any).
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the safelisted set.
    pub expose_headers: Vec<String>,
    /// Allow credentials; incompatible with a wildcard origin.
    pub allow_credentials: bool,
    /// `Access-Control-Max-Age` for preflights.
    pub max_age_secs: u64,
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
            max_age_secs: 600,
        }
    }
}

/// HTTP method accepted by a route's `match.http.methods`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    /// `GET`.
    #[serde(rename = "GET")]
    Get,
    /// `POST`.
    #[serde(rename = "POST")]
    Post,
    /// `PUT`.
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`.
    #[serde(rename = "DELETE")]
    Delete,
    /// `PATCH`.
    #[serde(rename = "PATCH")]
    Patch,
}

impl HttpMethod {
    /// The method as an HTTP token.
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

    /// Parses an HTTP method token, case-insensitively.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }
}

/// How the proxy URL's path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests that carry a path suffix.
    Disabled,
    /// Append the suffix to the route's path (default).
    #[default]
    Append,
}

/// `match.http`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpMatch {
    /// Methods accepted by this route.
    pub methods: Vec<HttpMethod>,
    /// Path prefix served by this route.
    pub path: String,
    /// Query parameters allowed through; empty allows none.
    pub query_allowlist: Vec<String>,
    /// How the proxy URL's path suffix is treated.
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

/// `match.grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    /// Fully-qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// `match` — exactly one of `http` / `grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP matching rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC matching rules (not proxied in this phase).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// Validates the exactly-one-of constraint.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when neither or both are present.
    pub fn validate(&self) -> Result<(), DomainError> {
        match (&self.http, &self.grpc) {
            (None, None) | (Some(_), Some(_)) => Err(DomainError::Validation(
                "match must declare exactly one of http or grpc".to_owned(),
            )),
            _ => Ok(()),
        }
    }
}

/// A stored upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstream {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Anonymous GTS resource id.
    pub gts_id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Routing key for `/oagw/v1/proxy/{alias}`.
    pub alias: String,
    /// Disabled upstreams answer `503`.
    pub enabled: bool,
    /// Add-only categorization tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: String,
    /// Outbound auth.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: HeadersConfig,
    /// Guard/transform plugin bindings.
    pub plugins: PluginsConfig,
    /// Rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
}

impl Upstream {
    /// Whether this upstream speaks HTTP (as opposed to gRPC).
    #[must_use]
    pub fn is_http_protocol(&self) -> bool {
        self.protocol == gts::PROTOCOL_HTTP
    }
}

/// A stored route.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Anonymous GTS resource id.
    pub gts_id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Upstream this route serves.
    pub upstream_id: Uuid,
    /// Disabled routes are excluded from matching.
    pub enabled: bool,
    /// Add-only categorization tags.
    pub tags: Vec<String>,
    /// Matching rules.
    pub match_config: MatchConfig,
    /// Match precedence among siblings: the lower the value, the earlier the
    /// route is considered when two prefixes have the same depth.
    pub priority: i64,
    /// Guard/transform plugin bindings.
    pub plugins: PluginsConfig,
    /// Rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy, overriding the upstream's when set.
    pub cors: Option<CorsConfig>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
}

/// A stored custom (Starlark) plugin definition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_field_names)] // `plugin_type` mirrors the wire field `type`
pub struct Plugin {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Anonymous GTS resource id.
    pub gts_id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Human-readable name.
    pub name: String,
    /// `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// JSON schema describing the plugin's config.
    pub config_schema: serde_json::Value,
    /// Starlark source.
    pub source_code: String,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
}

impl Plugin {
    /// `true` for the three documented plugin types.
    #[must_use]
    pub fn known_type(plugin_type: &str) -> bool {
        matches!(plugin_type, "auth" | "guard" | "transform")
    }
}

/// Now, formatted as RFC 3339.
#[must_use]
pub fn now_rfc3339() -> String {
    humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string()
}

/// Validates a tag per the schema's `^[a-z0-9_-]+$` pattern.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the tag is empty or carries a
/// character outside the pattern.
pub fn validate_tag(tag: &str) -> Result<(), DomainError> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(DomainError::Validation(format!(
            "invalid tag `{tag}`: must match ^[a-z0-9_-]+$"
        )));
    }
    Ok(())
}

/// Validates an alias per the schema's `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the shape does not match.
pub fn validate_alias_shape(alias: &str) -> Result<(), DomainError> {
    let bytes = alias.as_bytes();
    let ok_first = bytes
        .first()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let ok_last = bytes
        .last()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    let ok_middle = bytes.iter().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b':' || *b == b'-'
    });
    if bytes.is_empty() || !ok_first || !ok_last || !ok_middle {
        return Err(DomainError::Validation(format!("invalid alias `{alias}`")));
    }
    Ok(())
}

/// Validates a CORS origin (`*` or an absolute URI).
///
/// # Errors
/// Returns [`DomainError::Validation`] when the origin is neither `*` nor a
/// parseable absolute URI with a host.
pub fn validate_origin(origin: &str) -> Result<(), DomainError> {
    if origin == "*" {
        return Ok(());
    }
    url::Url::parse(origin)
        .ok()
        .filter(|u| u.host_str().is_some())
        .ok_or_else(|| DomainError::Validation(format!("invalid origin `{origin}`")))?;
    Ok(())
}
