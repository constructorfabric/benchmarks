// Created: 2026-09-02 by Constructor Tech
//! Domain model for the outbound API gateway control plane.
//!
//! The wire shapes here mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` field-for-field, including the
//! `additionalProperties: false` / `required` constraints: `Upstream` rejects
//! unknown members (its schema root sets `additionalProperties: false`) while
//! `Route` tolerates them (its root does not). Both schemas are the source of
//! truth, so the response body emits exactly the schema's property set.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::GatewayError;

/// Endpoint scheme, as accepted by `server.endpoints[].scheme`.
///
/// The JSON Schema enumerates `https|wss|wt|grpc` (default `https`). `http` is
/// additionally accepted: it is a *legal field value* everywhere, and only the
/// decision to actually dial a plaintext upstream is governed by the gear's
/// `allow_http_upstream` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[derive(Default)]
pub enum Scheme {
    /// Plaintext HTTP (`http` scheme).
    #[serde(rename = "http")]
    Http,
    /// HTTPS (default).
    #[serde(rename = "https")]
    #[default]
    Https,
    /// Secure WebSocket.
    #[serde(rename = "wss")]
    Wss,
    /// WebTransport.
    #[serde(rename = "wt")]
    Wt,
    /// gRPC over HTTP/2.
    #[serde(rename = "grpc")]
    Grpc,
}


impl Scheme {
    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }

    /// Standard port for the scheme (`HTTP: 80`, everything else `443`).
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether the scheme dials in plaintext (`http`, `wt` over http).
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http)
    }

    /// Whether the scheme speaks WebSocket (`wss`), or plain `http` carrying an
    /// `Upgrade` handshake.
    #[must_use]
    pub const fn supports_upgrade(self) -> bool {
        matches!(self, Self::Http | Self::Wss)
    }

    /// URL scheme used to dial the endpoint.
    #[must_use]
    pub const fn url_scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https | Self::Grpc => "https",
            // `wss` and WebTransport are dialled over TLS; WebTransport itself is
            // HTTP/3 and out of scope, so it is treated as HTTPS.
            Self::Wss | Self::Wt => "https",
        }
    }
}

/// Upstream protocol (`gts.cf.core.oagw.protocol.v1~…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// HTTP request/response proxying (HTTP/1.1 and HTTP/2).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC — Phase 3, no proxy code path is reachable today.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// Full GTS id on the wire.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => crate::gts::PROTOCOL_HTTP,
            Self::Grpc => crate::gts::PROTOCOL_GRPC,
        }
    }
}

/// Hierarchical sharing mode for a configuration field (`DESIGN.md` §3.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; descendants may override when permitted.
    Inherit,
    /// Visible; descendants cannot override.
    Enforce,
}

/// One upstream endpoint: scheme + host + optional port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Scheme. Defaults to `https` when omitted.
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname, IPv4 or IPv6 literal.
    #[serde(default)]
    pub host: String,
    /// Port; defaults to the scheme's standard port when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port (the scheme's standard port when unset).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.standard_port())
    }

    /// Host normalized to lowercase with a trailing FQDN dot stripped.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        normalize_host(&self.host)
    }

    /// `host` or `host:port` when the port is non-standard.
    #[must_use]
    pub fn host_port(&self) -> String {
        let host = self.normalized_host();
        let port = self.port();
        if port == self.scheme.standard_port() {
            host
        } else {
            format!("{host}:{}", port)
        }
    }

    /// Whether the host is an IP literal (v4 or bracketed v6).
    #[must_use]
    pub fn is_ip(&self) -> bool {
        is_ip_literal(&self.host)
    }
}

/// `server` object: one or more endpoints forming a load-balance pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoints of the pool (1..n). All must share scheme and port.
    pub endpoints: Vec<Endpoint>,
}

/// Auth plugin binding on an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier (`gts.cf.core.oagw.auth_plugin.v1~…`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    /// Sharing mode for hierarchical override.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin-specific configuration (`secret_ref`, header name, token endpoint…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
}

impl AuthConfig {
    /// Configuration value for `key`, if present.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.as_ref()?.get(key)?.as_str()
    }

    /// Configuration value for `key` coerced to a string (numbers included).
    #[must_use]
    pub fn config_value_str(&self, key: &str) -> Option<String> {
        match self.config.as_ref()?.get(key)? {
            serde_json::Value::String(s) => Some(s.clone()),
            other => Some(other.to_string()),
        }
    }

    /// The GTS plugin id, or `AUTH_NOOP` when the binding names no plugin.
    #[must_use]
    pub fn plugin_id(&self) -> Option<&str> {
        self.auth_type.as_deref().filter(|s| !s.is_empty())
    }
}

/// Which inbound headers are forwarded upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward none of the inbound headers (default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything except hop-by-hop and routing headers.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers set on the outbound request, overwriting any existing value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    /// Headers added to the outbound request (duplicates allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<std::collections::BTreeMap<String, String>>,
    /// Header names removed from the inbound request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
    /// Which inbound headers are forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<Passthrough>,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers set on the response to the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    /// Headers added to the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<std::collections::BTreeMap<String, String>>,
    /// Header names stripped from the upstream response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
}

/// `headers` object on an upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Outbound request rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    /// Response rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket with burst capacity (default).
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Rate-limit window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Window length as a [`Duration`].
    #[must_use]
    pub const fn duration(self) -> Duration {
        match self {
            Self::Second => Duration::from_secs(1),
            Self::Minute => Duration::from_secs(60),
            Self::Hour => Duration::from_secs(3600),
            Self::Day => Duration::from_secs(86_400),
        }
    }

    /// Window length in whole seconds.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Counter scope for a rate limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket shared by every caller.
    Global,
    /// One bucket per calling tenant (default).
    #[default]
    Tenant,
    /// One bucket per calling subject.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per matched route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with `429` and `Retry-After` (default).
    #[default]
    Reject,
    /// Queue the request, bounded by the upstream timeout.
    Queue,
    /// Serve the request and mark the response as degraded.
    Degrade,
}

/// Sustained rate: tokens replenished per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window unit; defaults to `second`.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstConfig {
    /// Bucket capacity (maximum burst size).
    pub capacity: u64,
}

/// Rate-limit configuration (`DESIGN.md` ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical override.
    #[serde(default)]
    pub sharing: Sharing,
    /// Algorithm; defaults to `token_bucket`.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (required).
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    /// Counter scope; defaults to `tenant`.
    #[serde(default)]
    pub scope: RateScope,
    /// Strategy on exceed; defaults to `reject`.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request; defaults to 1.
    #[serde(default)]
    pub cost: u64,
}

impl RateLimitConfig {
    /// Bucket capacity, defaulting to the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst.as_ref().map_or(self.sustained.rate, |b| b.capacity)
    }

    /// Replenishment period: `window / rate`.
    #[must_use]
    pub fn refill_period(&self) -> Duration {
        let window = self.sustained.window.duration();
        let rate = self.sustained.rate.max(1);
        // Sub-millisecond periods are clamped to 1ms: a token bucket cannot
        // meaningfully refill faster than the scheduler.
        let micros = window.as_micros() / u128::from(rate);
        Duration::from_micros(u64::try_from(micros).unwrap_or(1_000_000).max(1))
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode for hierarchical override.
    #[serde(default)]
    pub sharing: Sharing,
    /// Enable CORS for this upstream.
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods; defaults to `GET` and `POST`.
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed. Requires specific origins.
    #[serde(default)]
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// `GET` and `POST`, the CORS default method set.
#[must_use]
pub fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// Ordered plugin chain binding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Sharing mode for hierarchical override.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin references, in execution order. Each entry is either a bare
    /// identifier (a builtin GTS id or a custom plugin UUID) or an object
    /// carrying `plugin_ref` plus inline `config`, as `ADR-0009` binds them.
    #[serde(default)]
    pub items: Vec<serde_json::Value>,
}

/// HTTP match rules for a route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods accepted by this route (1..n).
    #[serde(default)]
    pub methods: Vec<HttpMethod>,
    /// Path pattern: the upstream path prefix this route serves.
    #[serde(default)]
    pub path: String,
    /// Allowed query parameters; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How the path suffix after `path` is treated. Defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: SuffixMode,
}

/// gRPC match rules for a route (Phase 3: persisted but not reachable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// `match` object: exactly one of `http` / `grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// How the path suffix after the route's `path` is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuffixMode {
    /// Append the suffix to `path` (default).
    #[default]
    Append,
    /// Reject requests that carry a suffix.
    Disabled,
}

/// HTTP methods a route may accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HttpMethod {
    /// `GET`
    #[serde(rename = "GET")]
    Get,
    /// `POST`
    #[serde(rename = "POST")]
    Post,
    /// `PUT`
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`
    #[serde(rename = "DELETE")]
    Delete,
    /// `PATCH`
    #[serde(rename = "PATCH")]
    Patch,
}

impl HttpMethod {
    /// Canonical wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }

    /// Parse a method name, case-insensitively.
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

/// The upstream wire shape (also the create / replace request body).
///
/// `id` is server-generated and therefore optional on input; `alias` is empty
/// when the caller omitted it (derivation decides whether that is legal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Server-generated GTS instance id (`gts.cf.core.oagw.upstream.v1~{uuid}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Enabled by default; a disabled upstream rejects every proxy request.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// Routing key. Auto-derived for hostname endpoints, explicit otherwise.
    #[serde(default)]
    pub alias: String,
    /// Discovery tags; effective tags are a hierarchy-wide add-only union.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Ordered guard/transform plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// The route wire shape (also the create / replace request body).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated GTS instance id (`gts.cf.core.oagw.route.v1~{uuid}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Owning upstream's GTS instance id (immutable once set).
    #[serde(default)]
    pub upstream_id: String,
    /// Match rules (`match` on the wire; exactly one of `http` / `grpc`).
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Ordered guard/transform plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

/// A custom (tenant-defined) plugin definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// Server-generated GTS instance id (`gts.cf.core.oagw.plugin.v1~{uuid}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Plugin type: which plugin registry resolves it.
    pub plugin_type: PluginType,
    /// Tenant-unique name.
    pub name: String,
    /// Starlark source, served verbatim by `GET /plugins/{id}/source`.
    pub source_code: String,
    /// Optional configuration schema, validated on bind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Tags (same shape as upstream/route tags).
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Which plugin registry a plugin belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Credential injection; one per upstream.
    Auth,
    /// Validation / policy enforcement; can reject a request.
    Guard,
    /// Request/response mutation; runs in order.
    Transform,
}

impl PluginType {
    /// Base type id for this plugin kind.
    #[must_use]
    pub const fn type_id(self) -> &'static str {
        match self {
            Self::Auth => crate::gts::AUTH_PLUGIN_TYPE,
            Self::Guard => crate::gts::GUARD_PLUGIN_TYPE,
            Self::Transform => crate::gts::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Parse a plugin kind from a GTS base type id.
    #[must_use]
    pub fn from_type_id(type_id: &str) -> Option<Self> {
        if type_id == crate::gts::AUTH_PLUGIN_TYPE {
            Some(Self::Auth)
        } else if type_id == crate::gts::GUARD_PLUGIN_TYPE {
            Some(Self::Guard)
        } else if type_id == crate::gts::TRANSFORM_PLUGIN_TYPE {
            Some(Self::Transform)
        } else {
            None
        }
    }
}

/// `true`, for serde defaults.
#[must_use]
pub fn default_true() -> bool {
    true
}

/// Normalizes a host or alias: ASCII lowercase, trailing FQDN dot stripped.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let trimmed = host.trim();
    let stripped = trimmed.strip_suffix('.').unwrap_or(trimmed);
    stripped.to_ascii_lowercase()
}

/// Whether `host` is an IPv4 or IPv6 literal (brackets tolerated).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let unbracketed = host.trim_start_matches('[').trim_end_matches(']');
    unbracketed.parse::<std::net::IpAddr>().is_ok()
}

/// Validates an endpoint host per RFC 1123 (`DESIGN.md` §3.2 Alias Enforcement).
///
/// Max 253 characters overall, labels of 1–63 ASCII alphanumeric/hyphen
/// characters that neither start nor end with a hyphen, and a tolerated
/// (stripped) trailing dot. IP literals are accepted as-is.
pub fn validate_host(host: &str) -> Result<(), GatewayError> {
    let invalid = || {
        GatewayError::Validation(format!(
            "endpoint host '{host}' is not a valid hostname or IP address"
        ))
    };
    if host.is_empty() {
        return Err(invalid());
    }
    if is_ip_literal(host) {
        return Ok(());
    }
    let normalized = normalize_host(host);
    if normalized.is_empty() || normalized.len() > 253 {
        return Err(invalid());
    }
    for label in normalized.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(invalid());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid());
        }
    }
    Ok(())
}

/// Validates a tag against `^[a-z0-9_-]+$`.
pub fn validate_tag(tag: &str) -> Result<(), GatewayError> {
    if tag.is_empty()
        || tag.len() > 64
        || !tag.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(GatewayError::Validation(format!(
            "tag '{tag}' must match ^[a-z0-9_-]+$"
        )));
    }
    Ok(())
}

/// Validates an alias against `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
pub fn validate_alias(alias: &str) -> Result<(), GatewayError> {
    let bytes = alias.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.first().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.last().is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(GatewayError::Validation(format!(
            "alias '{alias}' must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )))
    }
}

/// Derives the alias an endpoint pool is *required* to use, if it is derivable.
///
/// `None` means the caller must supply an explicit alias: IP-based endpoints,
/// heterogeneous hostnames with no registrable common suffix, or hostname pools
/// whose only common suffix is a bare public suffix.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    // A pool must share scheme and port, so the port suffix is unambiguous.
    let port = endpoints[0].port();
    let scheme = endpoints[0].scheme;
    let hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();
    let non_standard = |port: u16| port != scheme.standard_port();

    if endpoints.len() == 1 {
        let endpoint = &endpoints[0];
        if endpoint.is_ip() {
            return None;
        }
        return Some(if non_standard(endpoint.port()) {
            format!("{}:{port}", endpoint.normalized_host())
        } else {
            endpoint.normalized_host()
        });
    }

    if endpoints.iter().any(Endpoint::is_ip) {
        return None;
    }
    let suffix = common_domain_suffix(
        &hosts
            .iter()
            .map(|h| normalize_host(h))
            .collect::<Vec<_>>(),
    )?;
    Some(if non_standard(port) {
        format!("{suffix}:{port}")
    } else {
        suffix
    })
}

/// Registrable common suffix across `hosts` (`DESIGN.md` §3.2).
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    common_domain_suffix_inner(hosts)
}

fn common_domain_suffix_inner(hosts: &[String]) -> Option<String> {
    if hosts.len() < 2 {
        return None;
    }
    let normalized: Vec<String> = hosts.iter().map(|h| normalize_host(h)).collect();
    let reversed: Vec<Vec<&str>> = normalized
        .iter()
        .map(|h| h.split('.').rev().collect::<Vec<_>>())
        .collect();
    let first = reversed.first()?;
    let mut common: Vec<String> = Vec::new();
    for (idx, label) in first.iter().enumerate() {
        if reversed.iter().all(|labels| labels.get(idx) == Some(label)) {
            common.push((*label).to_owned());
        } else {
            break;
        }
    }
    if common.len() < 2 {
        return None;
    }
    common.reverse();
    let candidate = common.join(".");
    // Must be a registrable domain, not a bare public suffix.
    if psl::domain_str(&candidate) == Some(candidate.as_str()) {
        Some(candidate)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: Scheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint { scheme, host: host.to_owned(), port }
    }

    #[test]
    fn scheme_defaults_and_standard_ports() {
        assert_eq!(Scheme::default(), Scheme::Https);
        assert_eq!(Scheme::Http.standard_port(), 80);
        assert_eq!(Scheme::Https.standard_port(), 443);
        assert_eq!(Scheme::Wss.standard_port(), 443);
        assert_eq!(Scheme::Grpc.standard_port(), 443);
        assert!(Scheme::Http.is_plaintext());
        assert!(!Scheme::Https.is_plaintext());
        assert!(Scheme::Http.supports_upgrade());
        assert!(Scheme::Wss.supports_upgrade());
        assert!(!Scheme::Grpc.supports_upgrade());
    }

    #[test]
    fn endpoint_port_defaults_to_scheme() {
        let e = ep(Scheme::Http, "Example.COM.", None);
        assert_eq!(e.port(), 80);
        assert_eq!(e.normalized_host(), "example.com");
        assert_eq!(e.host_port(), "example.com");
        let e = ep(Scheme::Https, "api.openai.com", Some(8443));
        assert_eq!(e.host_port(), "api.openai.com:8443");
    }

    #[test]
    fn alias_is_derived_from_a_single_hostname() {
        let derived = compute_derived_alias(&[ep(Scheme::Https, "api.openai.com", Some(443))]);
        assert_eq!(derived.as_deref(), Some("api.openai.com"));
        let derived = compute_derived_alias(&[ep(Scheme::Https, "api.openai.com", Some(8443))]);
        assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
        let derived = compute_derived_alias(&[ep(Scheme::Http, "api.local", Some(80))]);
        assert_eq!(derived.as_deref(), Some("api.local"));
    }

    #[test]
    fn ip_endpoints_require_an_explicit_alias() {
        let derived = compute_derived_alias(&[ep(Scheme::Https, "10.0.1.1", None)]);
        assert_eq!(derived, None);
        let derived = compute_derived_alias(&[
            ep(Scheme::Https, "10.0.1.1", None),
            ep(Scheme::Https, "10.0.1.2", None),
        ]);
        assert_eq!(derived, None);
        // IPv6 literal.
        let derived = compute_derived_alias(&[ep(Scheme::Https, "::1", None)]);
        assert_eq!(derived, None);
    }

    #[test]
    fn multi_endpoint_alias_uses_the_registrable_common_suffix() {
        let derived = compute_derived_alias(&[
            ep(Scheme::Https, "us.vendor.com", Some(443)),
            ep(Scheme::Https, "eu.vendor.com", Some(443)),
        ]);
        assert_eq!(derived.as_deref(), Some("vendor.com"));
        // Non-standard port is preserved in the alias.
        let derived = compute_derived_alias(&[
            ep(Scheme::Https, "us.vendor.com", Some(8443)),
            ep(Scheme::Https, "eu.vendor.com", Some(8443)),
        ]);
        assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn a_bare_public_suffix_is_not_derivable() {
        let derived = compute_derived_alias(&[
            ep(Scheme::Https, "foo.co.uk", None),
            ep(Scheme::Https, "bar.co.uk", None),
        ]);
        assert_eq!(derived, None, "co.uk is a public suffix, not a registrable domain");
    }

    #[test]
    fn heterogeneous_hostnames_require_an_explicit_alias() {
        let derived = compute_derived_alias(&[
            ep(Scheme::Https, "us.foo.com", None),
            ep(Scheme::Https, "eu.bar.com", None),
        ]);
        assert_eq!(derived, None);
    }

    #[test]
    fn host_validation_rejects_bad_labels() {
        assert!(validate_host("api.openai.com").is_ok());
        assert!(validate_host("API.OpenAI.COM.").is_ok());
        assert!(validate_host("127.0.0.1").is_ok());
        assert!(validate_host("[::1]").is_ok());
        assert!(validate_host("-bad.example.com").is_err());
        assert!(validate_host("bad-.example.com").is_err());
        assert!(validate_host("bad_.example.com").is_err());
        assert!(validate_host("").is_err());
        assert!(validate_host("a..b").is_err());
        assert!(validate_host(&"a".repeat(64)).is_err());
    }

    #[test]
    fn alias_validation_enforces_the_schema_pattern() {
        assert!(validate_alias("api.openai.com").is_ok());
        assert!(validate_alias("my-service").is_ok());
        assert!(validate_alias("api.openai.com:8443").is_ok());
        assert!(validate_alias("-leading").is_err());
        assert!(validate_alias("trailing-").is_err());
        assert!(validate_alias("").is_err());
        assert!(validate_alias("UPPER").is_err());
        assert!(validate_alias("spa ce").is_err());
    }

    #[test]
    fn tags_must_match_the_schema_pattern() {
        assert!(validate_tag("openai").is_ok());
        assert!(validate_tag("llm-v2").is_ok());
        assert!(validate_tag("LLM").is_err());
        assert!(validate_tag("a b").is_err());
        assert!(validate_tag("").is_err());
    }

    #[test]
    fn rate_limit_defaults_burst_to_the_sustained_rate() {
        let cfg = RateLimitConfig {
            sharing: Sharing::default(),
            algorithm: RateLimitAlgorithm::default(),
            sustained: SustainedRate { rate: 5, window: RateWindow::Minute },
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
        };
        assert_eq!(cfg.capacity(), 5);
        assert_eq!(cfg.refill_period(), Duration::from_secs(12));
    }

    #[test]
    fn cors_defaults_to_the_schema_defaults() {
        let cors = CorsConfig::default();
        assert!(!cors.enabled);
        assert_eq!(cors.allowed_methods, vec!["GET", "POST"]);
        assert!(!cors.allow_credentials);
    }

    #[test]
    fn plugin_type_is_derived_from_the_gts_base_type() {
        assert_eq!(
            PluginType::from_type_id(crate::gts::GUARD_PLUGIN_TYPE),
            Some(PluginType::Guard)
        );
        assert_eq!(PluginType::from_type_id("gts.cf.core.other.v1~"), None);
    }

    #[test]
    fn http_methods_round_trip() {
        for (raw, expected) in [
            ("GET", HttpMethod::Get),
            ("post", HttpMethod::Post),
            ("Put", HttpMethod::Put),
            ("DELETE", HttpMethod::Delete),
            ("patch", HttpMethod::Patch),
        ] {
            assert_eq!(HttpMethod::parse(raw), Some(expected));
            assert_eq!(HttpMethod::parse(expected.as_str()), Some(expected));
        }
        assert_eq!(HttpMethod::parse("OPTIONS"), None);
    }
}
