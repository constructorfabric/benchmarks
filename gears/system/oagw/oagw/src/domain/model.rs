//! Domain model for the `oagw` gear.
//!
//! The wire shape follows `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`; `http` is additionally accepted as an
//! endpoint scheme (the change request makes it a legal create-time value —
//! only the *connection* is gated by `allow_http_upstream`).

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids;

/// Seconds since the Unix epoch.
pub type UnixSeconds = u64;

/// Endpoint transport scheme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP (accepted at create time; connect-time gated by
    /// `allow_http_upstream`).
    Http,
    /// HTTP over TLS.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
}

impl Scheme {
    /// Default port for the scheme: 80 for plaintext HTTP, 443 otherwise.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Endpoint {
    /// URI scheme.
    #[serde(default)]
    pub scheme: Scheme,
    /// RFC 1123 hostname or IP literal.
    pub host: String,
    /// TCP port (defaults to the scheme's standard port).
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    Scheme::default().standard_port()
}

impl Endpoint {
    /// `host` or `host:port` depending on whether the port is standard.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.port == self.scheme.standard_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `server` block of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ServerConfig {
    /// Endpoint pool. All endpoints must share the same scheme and port.
    pub endpoints: Vec<Endpoint>,
}

/// Upstream protocol, addressed by its GTS identifier on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// GTS identifier for this protocol.
    #[must_use]
    pub const fn as_gts_id(self) -> &'static str {
        match self {
            Self::Http => ids::PROTOCOL_HTTP,
            Self::Grpc => ids::PROTOCOL_GRPC,
        }
    }
}

/// Hierarchical sharing mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override; always participates in merges.
    Enforce,
}

/// Which inbound headers are forwarded upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward nothing (default — only `set`/`add` headers reach upstream).
    #[default]
    None,
    /// Forward only `passthrough_allowlist` entries.
    Allowlist,
    /// Forward everything except routing and hop-by-hop headers.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RequestHeaderRules {
    /// Headers to set (overwriting an existing value).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (appending, allowing duplicates).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to drop from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: Passthrough,
    /// Forwarded headers when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResponseHeaderRules {
    /// Headers to set on the client response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to append to the client response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to drop from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeadersConfig {
    /// Rules applied to the outbound (upstream) request.
    #[serde(default)]
    pub request: RequestHeaderRules,
    /// Rules applied to the client response.
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

/// Auth plugin binding for an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Sharing mode for hierarchical overrides.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// A plugin binding as submitted on the wire: either a bare plugin reference
/// or a reference plus its configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBindingSpec {
    /// Bare reference (`gts.cf.core.oagw.guard_plugin.v1~…` or a UUID).
    Reference(String),
    /// Reference with configuration.
    Detailed {
        /// Plugin reference.
        plugin_ref: String,
        /// Plugin configuration.
        #[serde(default)]
        config: serde_json::Value,
    },
}

/// Normalized plugin binding stored on an upstream or route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginBinding {
    /// Canonical plugin reference (full GTS identifier or UUID).
    pub plugin_ref: String,
    /// Plugin configuration.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

impl PluginBinding {
    /// Build a binding from its wire form.
    #[must_use]
    pub fn from_spec(spec: PluginBindingSpec) -> Self {
        match spec {
            PluginBindingSpec::Reference(plugin_ref) => Self {
                plugin_ref,
                config: serde_json::Value::Null,
            },
            PluginBindingSpec::Detailed { plugin_ref, config } => Self { plugin_ref, config },
        }
    }
}

/// Plugin chain block.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginsConfig {
    /// Sharing mode for hierarchical overrides.
    #[serde(default)]
    pub sharing: Sharing,
    /// Ordered plugin bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

/// Sustained-rate component of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SustainedRate {
    /// Tokens replenished per window.
    #[serde(default = "default_rate")]
    pub rate: u32,
    /// Window size.
    #[serde(default)]
    pub window: RateWindow,
}

fn default_rate() -> u32 {
    1
}

/// Rate-limit window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Window length in seconds.
    #[must_use]
    pub const fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Burst component of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Burst {
    /// Bucket capacity.
    pub capacity: u32,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket (allows bursts).
    #[default]
    TokenBucket,
    /// Sliding window (prevents boundary bursts).
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One counter for the whole gear.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Wait (bounded) for a token, then reject with `429`.
    Queue,
    /// Admit the request but mark it as degraded.
    Degrade,
}

/// Dual-rate limit configuration (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical overrides.
    #[serde(default)]
    pub sharing: Sharing,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (required).
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default)]
    pub burst: Option<Burst>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// Behaviour on exhaustion.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Emit `X-RateLimit-*` response headers.
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

fn default_cost() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

impl RateLimitConfig {
    /// Effective burst capacity (defaults to `sustained.rate`).
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        match self.burst {
            Some(burst) => burst.capacity,
            None => self.sustained.rate,
        }
    }

    /// Tokens replenished per second.
    #[must_use]
    pub fn tokens_per_second(&self) -> f64 {
        f64::from(self.sustained.rate.max(1))
            / f64::from(u32::try_from(self.sustained.window.secs()).unwrap_or(1))
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CorsConfig {
    /// Sharing mode for hierarchical overrides.
    #[serde(default)]
    pub sharing: Sharing,
    /// Enable CORS handling for this upstream/route.
    pub enabled: bool,
    /// Allowed origins; `*` allows any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(
        default = "default_allowed_methods",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials. Requires a specific origin (never `*`).
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl CorsConfig {
    /// Whether `origin` is allowed. `*` matches every origin.
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
    }

    /// Whether `method` is allowed (case-insensitive).
    #[must_use]
    pub fn method_allowed(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// Fields of an upstream that participate in create/replace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamSpec {
    /// Whether the upstream serves traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Flat tags for categorization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Optional auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_default")]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_default")]
    pub plugins: PluginsConfig,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for UpstreamSpec {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: Vec::new(),
            },
            protocol: Protocol::Http,
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

impl HeadersConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl PluginsConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Fields of a route that participate in create/replace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteSpec {
    /// Whether the route serves traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Flat tags for categorization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Protocol-scoped inbound matching rules.
    pub r#match: RouteMatch,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_default")]
    pub plugins: PluginsConfig,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Match priority; higher wins for equal path length.
    #[serde(default)]
    pub priority: i32,
}

impl Default for RouteSpec {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            tags: Vec::new(),
            r#match: RouteMatch::Http(HttpMatch::default()),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            priority: 0,
        }
    }
}

/// Protocol-scoped match rules (exactly one of `http`/`grpc`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMatch {
    /// HTTP matching rules.
    Http(HttpMatch),
    /// gRPC matching rules.
    Grpc(GrpcMatch),
}

impl RouteMatch {
    /// Canonical, comparable key for this rule: the protocol discriminant
    /// plus every field that participates in matching. Two routes for the
    /// same upstream with the same key and priority are a uniqueness conflict.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Http(rules) => {
                let mut methods: Vec<&str> = rules.methods.iter().map(String::as_str).collect();
                methods.sort_unstable();
                methods.dedup();
                format!(
                    "http|{}|{}|{}|{}",
                    methods.join(","),
                    rules.path_suffix_mode.as_str(),
                    rules.path,
                    rules.query_allowlist.join(",")
                )
            }
            Self::Grpc(rules) => format!("grpc|{}|{}", rules.service, rules.method),
        }
    }
}

/// HTTP matching rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HttpMatch {
    /// Allowed methods (at least one).
    #[serde(default = "default_methods")]
    pub methods: Vec<String>,
    /// Path pattern for the route.
    #[serde(default)]
    pub path: String,
    /// Allowed query parameters; empty allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

fn default_methods() -> Vec<String> {
    vec!["GET".to_owned()]
}

/// Path-suffix handling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the suffix to `path` (default).
    #[default]
    Append,
}

impl PathSuffixMode {
    /// Stable string form used in match-rule keys.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Append => "append",
        }
    }
}

/// gRPC matching rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GrpcMatch {
    /// Fully qualified service name.
    #[serde(default)]
    pub service: String,
    /// RPC method name.
    #[serde(default)]
    pub method: String,
}

/// A stored upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Upstream {
    /// System-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Normalized routing key.
    pub alias: String,
    /// Whether the alias was supplied by the operator rather than derived.
    pub alias_explicit: bool,
    /// Configuration payload.
    #[serde(flatten)]
    pub spec: UpstreamSpec,
    /// Creation timestamp (Unix seconds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_at: UnixSeconds,
    /// Last modification timestamp (Unix seconds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub updated_at: UnixSeconds,
}

/// A stored route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Route {
    /// System-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Referenced upstream (immutable after create).
    pub upstream_id: Uuid,
    /// Configuration payload.
    #[serde(flatten)]
    pub spec: RouteSpec,
    /// Creation timestamp (Unix seconds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_at: UnixSeconds,
    /// Last modification timestamp (Unix seconds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub updated_at: UnixSeconds,
}

/// Plugin kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Credential injection.
    Auth,
    /// Policy enforcement.
    Guard,
    /// Request/response mutation.
    Transform,
}

impl PluginType {
    /// GTS base type for this plugin kind.
    #[must_use]
    pub const fn base_type(self) -> &'static str {
        match self {
            Self::Auth => ids::AUTH_PLUGIN_TYPE,
            Self::Guard => ids::GUARD_PLUGIN_TYPE,
            Self::Transform => ids::TRANSFORM_PLUGIN_TYPE,
        }
    }
}

/// A stored (custom) plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Plugin {
    /// System-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Human-readable plugin name.
    pub name: String,
    /// Plugin kind.
    #[serde(rename = "type")]
    pub kind: PluginType,
    /// Optional JSON Schema for the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source of the plugin.
    #[serde(default)]
    pub source_code: String,
    /// Creation timestamp (Unix seconds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub created_at: UnixSeconds,
    /// Last modification timestamp (Unix seconds).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub updated_at: UnixSeconds,
}

/// Current Unix time in seconds.
#[must_use]
pub fn now_unix() -> UnixSeconds {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// `serde`'s `skip_serializing_if` always calls the predicate with a reference.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde calls the predicate through `&T`"
)]
fn is_zero(value: &UnixSeconds) -> bool {
    *value == 0
}

/// Validate an RFC 1123 hostname or an IP literal.
///
/// Returns the normalized host (lowercased, trailing dot stripped).
///
/// # Errors
///
/// Returns a [`crate::domain::error::DomainError`] validation error when the
/// host is empty, too long, or contains invalid labels.
pub fn validate_host(host: &str) -> Result<String, crate::domain::error::DomainError> {
    let trimmed = host.trim_end_matches('.').to_ascii_lowercase();
    if trimmed.is_empty() || trimmed.len() > 253 {
        return Err(crate::domain::error::DomainError::validation(format!(
            "invalid endpoint host `{host}`: expected 1-253 characters"
        )));
    }
    if trimmed.parse::<IpAddr>().is_ok() {
        return Ok(trimmed);
    }
    for label in trimmed.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(crate::domain::error::DomainError::validation(format!(
                "invalid endpoint host `{host}`: label length out of range"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || label.starts_with('-')
            || label.ends_with('-')
        {
            return Err(crate::domain::error::DomainError::validation(format!(
                "invalid endpoint host `{host}`: label `{label}` is not RFC 1123"
            )));
        }
    }
    Ok(trimmed)
}

/// Whether `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok()
}

/// Validate a tag against `^[a-z0-9_-]+$`.
///
/// # Errors
///
/// Returns a validation error when the tag is malformed.
pub fn validate_tag(tag: &str) -> Result<(), crate::domain::error::DomainError> {
    let valid = !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(crate::domain::error::DomainError::validation(format!(
            "invalid tag `{tag}`: expected ^[a-z0-9_-]+$"
        )))
    }
}

/// Validate an alias against `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
///
/// Returns a validation error when the alias is malformed.
pub fn validate_alias(alias: &str) -> Result<(), crate::domain::error::DomainError> {
    use crate::domain::error::DomainError;
    let bytes = alias.as_bytes();
    let ok = !alias.is_empty()
        && alias.len() <= 253
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(DomainError::validation(format!(
            "invalid alias `{alias}`: expected ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )))
    }
}

/// Normalize an alias: ASCII lowercase with trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Parse an IP literal, tolerating the bracketed IPv6 form.
#[must_use]
pub fn parse_ip(host: &str) -> Option<IpAddr> {
    IpAddr::from_str(host.trim_matches(['[', ']'])).ok()
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod model_tests;
