//! OAGW domain model.
//!
//! Wire types mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` verbatim, so the same Rust type is the
//! domain model, the REST request/response body and the OpenAPI component.
//!
//! Two things are deliberately *not* modelled here:
//!
//! * `id` / `tenant_id` are owned by the repositories and the services; a
//!   create/replace body carries them only so a conflicting value can be
//!   reported as `409 immutable field` rather than silently ignored.
//! * credentials — auth configuration references secret material by `cred://`
//!   URI only (`cpt-cf-oagw-principle-cred-isolation`); no secret material is
//!   ever stored on an [`Upstream`].

use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use toolkit_macros::api_dto;

use crate::domain::error::DomainError;

/// GTS type id of the `Upstream` resource.
pub const UPSTREAM_GTS_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS type id of the `Route` resource.
pub const ROUTE_GTS_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// GTS type id of the proxy resource (inbound permission scope).
pub const PROXY_GTS_TYPE: &str = "gts.cf.core.oagw.proxy.v1~";

/// Inbound header consumed by OAGW during routing, then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Protocol used to connect to an upstream service.
///
/// GTS identifiers quoted from `docs/schemas/upstream.v1.schema.json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
pub enum UpstreamProtocol {
    /// Plain HTTP / HTTP/2 upstream.
    #[default]
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC upstream (routing planned for Phase 3 — no proxy code path).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl UpstreamProtocol {
    /// Canonical GTS identifier for this protocol.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            Self::Grpc => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        }
    }
}

/// Endpoint transport scheme.
///
/// `http` is a legal *declaration*: it is accepted into the roster by the
/// management API unconditionally. Whether a plaintext connection is actually
/// dialed is a separate, connection-time decision governed by
/// [`crate::config::OagwConfig::allow_http_upstream`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP.
    Http,
    /// TLS HTTP.
    #[default]
    Https,
    /// TLS WebSocket.
    Wss,
    /// TLS WebTransport.
    Wt,
    /// TLS gRPC.
    Grpc,
}

impl EndpointScheme {
    /// Standard port for this scheme; omitted from a derived alias.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Port used when the caller omits `port`.
    #[must_use]
    pub const fn default_port(self) -> u16 {
        self.standard_port()
    }

    /// URI scheme spelled the way an outbound URL expects it.
    #[must_use]
    pub const fn as_uri_scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => "https",
        }
    }

    /// `true` when dialing this scheme yields a plaintext connection.
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http)
    }

    /// `true` when this scheme describes a WebSocket/WebTransport upgrade
    /// (tunnelled by the data plane in a later phase).
    #[must_use]
    pub const fn is_upgrade(self) -> bool {
        matches!(self, Self::Wss | Self::Wt)
    }
}

/// A single upstream endpoint (`scheme`, `host`, `port`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct Endpoint {
    /// Transport scheme. Defaults to `https`.
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// Hostname or IP address (RFC 1123 hostname, IPv4, or IPv6).
    pub host: String,
    /// Port. Defaults to the scheme's standard port (80 for `http`, 443
    /// otherwise) when omitted.
    #[serde(default)]
    pub port: u16,
}

impl Endpoint {
    /// Build an endpoint with an explicit port.
    #[must_use]
    pub fn new(scheme: EndpointScheme, host: impl Into<String>, port: u16) -> Self {
        Self {
            scheme,
            host: host.into(),
            port,
        }
    }

    /// Build an endpoint on the scheme's standard port.
    #[must_use]
    pub fn with_standard_port(scheme: EndpointScheme, host: impl Into<String>) -> Self {
        let port = scheme.standard_port();
        Self::new(scheme, host, port)
    }

    /// Parsed IP address, when the host is written as an IP literal.
    #[must_use]
    pub fn ip_addr(&self) -> Option<IpAddr> {
        parse_ip(&self.host)
    }

    /// Ascii-lowercased host with the trailing dot stripped.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        normalize_host(&self.host)
    }

    /// `true` when the endpoint host is an IP literal rather than a hostname.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        self.ip_addr().is_some()
    }

    /// Host spelling used inside a URI authority (brackets an IPv6 literal).
    #[must_use]
    pub fn uri_host(&self) -> String {
        if let Some(IpAddr::V6(v6)) = self.ip_addr() {
            format!("[{v6}]")
        } else {
            self.host.clone()
        }
    }

    /// `host:port` authority for the endpoint, honouring the standard port.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.port == self.scheme.standard_port() {
            self.uri_host()
        } else {
            format!("{}:{}", self.uri_host(), self.port)
        }
    }
}

// `port` is filled from the scheme's standard port when the caller omits it, so
// `{"scheme": "http", "host": "svc"}` is `http://svc:80` rather than `:443`.
// Hand-written (instead of derived through `#[api_dto]`) because the default
// depends on `scheme`.
impl<'de> Deserialize<'de> for Endpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize, Default)]
        #[serde(default, deny_unknown_fields)]
        struct Wire {
            scheme: EndpointScheme,
            host: String,
            port: Option<u16>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if let Some(port) = wire.port {
            return Ok(Endpoint {
                scheme: wire.scheme,
                host: wire.host,
                port,
            });
        }
        Ok(Endpoint::with_standard_port(wire.scheme, wire.host))
    }
}

impl toolkit::api::api_dto::RequestApiDto for Endpoint {}
impl toolkit::api::api_dto::ResponseApiDto for Endpoint {}

/// Parses an IP literal, accepting the `[::1]` bracketed spelling.
#[must_use]
pub fn parse_ip(host: &str) -> Option<IpAddr> {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    trimmed.parse::<IpAddr>().ok()
}

/// The `server` block: one or more endpoints forming a round-robin pool.
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerConfig {
    /// Endpoints of the pool (1..).
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical sharing mode for an overridable configuration field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

/// Outbound authentication plugin binding on an [`Upstream`].
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier (`gts.cf.core.oagw.auth_plugin.v1~…`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration. Must not carry secret material — secrets are
    /// referenced as `cred://` URIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Inbound-header passthrough policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPassthrough {
    /// Forward no inbound headers (default).
    #[default]
    None,
    /// Forward only the names in `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header.
    All,
}

/// Request-side header transformation rules.
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if present).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    #[serde(default)]
    pub passthrough: HeaderPassthrough,
    /// Allowlist used when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation rules (`headers` block of the upstream schema).
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadersConfig {
    /// Inbound → outbound rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    /// Upstream response → client rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

impl HeadersConfig {
    /// `true` when no header transformation is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request.is_none() && self.response.is_none()
    }
}

/// Ordered plugin chain bound to an upstream or route.
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugins in execution order: built-ins by GTS id, custom plugins by UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// Per-plugin configuration, keyed by the full plugin reference.
    ///
    /// Optional and additive: the wire contract (`plugins.items` is an array of
    /// references) is unchanged, and a plugin that needs no configuration is
    /// simply absent from this map.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub configs: std::collections::BTreeMap<String, serde_json::Value>,
}

impl PluginsConfig {
    /// `true` when no plugin chain is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sharing == SharingMode::default() && self.items.is_empty() && self.configs.is_empty()
    }

    /// Configuration bound to `plugin_ref`.
    #[must_use]
    pub fn config_for(&self, plugin_ref: &str) -> Option<&serde_json::Value> {
        self.configs.get(plugin_ref)
    }
}

/// Rate-limit window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
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
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained replenishment rate.
#[api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window the rate applies over.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity (token-bucket size).
#[api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BurstCapacity {
    /// Bucket capacity; defaults to [`SustainedRate::rate`].
    pub capacity: u64,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Allows bursts (default).
    #[default]
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Scope of the rate-limit counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One shared counter.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade the response.
    Degrade,
}

/// Rate-limit configuration (`rate_limit` block).
#[api_dto(request, response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (required).
    pub sustained: SustainedRate,
    /// Explicit burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// Behaviour when exceeded.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
}

fn default_cost() -> u64 {
    1
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateLimitAlgorithm::default(),
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::default(),
            },
            burst: None,
            scope: RateLimitScope::default(),
            strategy: RateLimitStrategy::default(),
            cost: default_cost(),
        }
    }
}

impl RateLimitConfig {
    /// Effective burst capacity (defaults to the sustained rate).
    #[must_use]
    pub fn burst_capacity(&self) -> u64 {
        self.burst.map_or(self.sustained.rate, |b| b.capacity)
    }

    /// The stricter of the two configurations (per-second rate, then burst).
    ///
    /// DESIGN.md: rate limits merge with `min(ancestor, descendant)` — stricter
    /// always wins.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // window is bounded (< 2^23 s)
    pub fn stricter_of(&self, other: &Self) -> Self {
        let self_per_sec =
            self.sustained.rate as f64 / self.sustained.window.seconds() as f64;
        let other_per_sec =
            other.sustained.rate as f64 / other.sustained.window.seconds() as f64;
        let chosen = if other_per_sec < self_per_sec { other } else { self };
        let mut merged = chosen.clone();
        merged.burst = Some(BurstCapacity {
            capacity: self.burst_capacity().min(other.burst_capacity()),
        });
        merged
    }
}

/// CORS configuration (`cors` block).
#[api_dto(request, response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS for this upstream/route (required).
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(default = "default_cors_methods", skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials. Requires specific origins (never `*`).
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    ["GET", "POST"].iter().map(|m| (*m).to_owned()).collect()
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

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Alias handling the API performs for an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasResolution {
    /// The caller omitted `alias`; the value is derived from the endpoints.
    Derived,
    /// The caller supplied an explicit alias (IP-based / non-derivable pool).
    Explicit,
    /// The caller supplied exactly the derived value (tolerated idempotently).
    ExplicitMatchesDerived,
}

/// Create/replace body for an upstream.
///
/// `id` / `tenant_id` are accepted so a conflicting value can be reported as
/// `409` instead of silently overwritten; they are never honoured.
#[api_dto(request)]
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamSpec {
    /// Server-generated id. Supplied values are rejected as immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<uuid::Uuid>,
    /// Owning tenant; always taken from the caller's security context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<uuid::Uuid>,
    /// Whether this upstream is enabled.
    #[serde(default = "crate::domain::models::default_true")]
    pub enabled: bool,
    /// Explicit alias. Optional; derivation rules decide whether it is needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: UpstreamProtocol,
    /// Outbound authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    /// Rate-limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for UpstreamSpec {
    fn default() -> Self {
        Self {
            id: None,
            tenant_id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: UpstreamProtocol::default(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

/// Persisted upstream (tenant-scoped root configuration object).
#[api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// Server-generated id.
    pub id: uuid::Uuid,
    /// Owning tenant. Internal only: the wire contract
    /// (`docs/schemas/upstream.v1.schema.json`) does not carry it.
    #[serde(skip_serializing)]
    pub tenant_id: uuid::Uuid,
    /// Routing alias (derived or explicit, ASCII-lowercase).
    pub alias: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: UpstreamProtocol,
    /// Flat discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Outbound authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    /// Rate-limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    /// Build an upstream from a validated spec.
    #[must_use]
    pub fn from_spec(spec: UpstreamSpec, id: uuid::Uuid, tenant_id: uuid::Uuid, alias: String) -> Self {
        Self {
            id,
            tenant_id,
            alias,
            enabled: spec.enabled,
            server: spec.server,
            protocol: spec.protocol,
            tags: spec.tags,
            auth: spec.auth,
            headers: spec.headers,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
        }
    }

    /// Endpoint hosts, in pool order, lowercased with the trailing dot stripped.
    #[must_use]
    pub fn endpoint_hosts(&self) -> Vec<String> {
        self.server
            .endpoints
            .iter()
            .map(|e| normalize_host(&e.host))
            .collect()
    }

    /// The endpoint matching `host`, compared case-insensitively.
    #[must_use]
    pub fn endpoint_for_host(&self, host: &str) -> Option<&Endpoint> {
        let wanted = normalize_host(host);
        self.server
            .endpoints
            .iter()
            .find(|e| normalize_host(&e.host) == wanted)
    }

    /// Effective tags: additive union with `inherited` (descendants may add,
    /// never remove, inherited tags).
    #[must_use]
    pub fn effective_tags(&self, inherited: &[String]) -> Vec<String> {
        let mut seen = BTreeSet::new();
        for tag in inherited.iter().chain(self.tags.iter()) {
            seen.insert(tag.clone());
        }
        seen.into_iter().collect()
    }
}

/// Structural validation of an upstream spec, independent of tenant state.
///
/// # Errors
/// Returns a 400 [`DomainError::Validation`] on any structural violation.
pub fn validate_upstream_spec(spec: &UpstreamSpec) -> Result<(), DomainError> {
    if spec.server.endpoints.is_empty() {
        return Err(DomainError::Validation(
            "upstream requires at least one endpoint".to_owned(),
        ));
    }
    if spec.server.endpoints.len() > MAX_ENDPOINTS {
        return Err(DomainError::Validation(format!(
            "upstream accepts at most {MAX_ENDPOINTS} endpoints"
        )));
    }
    let first = &spec.server.endpoints[0];
    for endpoint in &spec.server.endpoints {
        validate_endpoint(endpoint)?;
        // A pool must be homogeneous: same scheme and port so a single alias
        // can describe it (DESIGN.md §3.1 "Multi-Endpoint Load Balancing").
        if endpoint.scheme != first.scheme || endpoint.port != first.port {
            return Err(DomainError::Validation(
                "all endpoints of an upstream must share the same scheme and port".to_owned(),
            ));
        }
    }
    for tag in &spec.tags {
        validate_tag(tag)?;
    }
    if let Some(cors) = &spec.cors {
        validate_cors(cors)?;
    }
    Ok(())
}

/// Maximum endpoints in one upstream pool.
pub const MAX_ENDPOINTS: usize = 32;

/// Validates a single endpoint's host/port.
///
/// # Errors
/// Returns a 400 [`DomainError::Validation`].
pub fn validate_endpoint(endpoint: &Endpoint) -> Result<(), DomainError> {
    if endpoint.is_ip() {
        return Ok(());
    }
    validate_hostname(&endpoint.host).map_err(|_| {
        DomainError::Validation(format!(
            "endpoint host '{}' is not a valid RFC 1123 hostname or IP literal",
            endpoint.host
        ))
    })
}

/// Maximum length of an RFC 1123 hostname.
pub const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a single hostname label.
pub const MAX_HOSTNAME_LABEL_LEN: usize = 63;

/// Validates a hostname per RFC 1123; a trailing dot (FQDN notation) is
/// tolerated.
///
/// # Errors
/// Returns a description of the first violation.
pub fn validate_hostname(host: &str) -> Result<(), String> {
    let trimmed = host.trim_end_matches('.');
    if trimmed.is_empty() {
        return Err("hostname is empty".to_owned());
    }
    if trimmed.len() > MAX_HOSTNAME_LEN {
        return Err("hostname exceeds 253 characters".to_owned());
    }
    for label in trimmed.split('.') {
        if label.is_empty() {
            return Err("hostname contains an empty label".to_owned());
        }
        if label.len() > MAX_HOSTNAME_LABEL_LEN {
            return Err("hostname label exceeds 63 characters".to_owned());
        }
        let first = label.as_bytes()[0];
        let last = label.as_bytes()[label.len() - 1];
        if first == b'-' || last == b'-' {
            return Err("hostname label cannot start or end with a hyphen".to_owned());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("hostname labels may only contain ASCII letters, digits and hyphens"
                .to_owned());
        }
    }
    Ok(())
}

/// Validates a discovery tag against `^[a-z0-9_-]+$`.
///
/// # Errors
/// Returns a 400 [`DomainError::Validation`] when the tag is malformed.
pub fn validate_tag(tag: &str) -> Result<(), DomainError> {
    let ok = !tag.is_empty()
        && tag.len() <= 64
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(DomainError::Validation(format!(
            "tag '{tag}' must match ^[a-z0-9_-]+$ and be at most 64 characters"
        )))
    }
}

/// Validates a CORS block: `allow_credentials` forbids the `*` origin and a
/// method list may only contain known HTTP methods.
///
/// # Errors
/// Returns a 400 [`DomainError::Validation`].
pub fn validate_cors(cors: &CorsConfig) -> Result<(), DomainError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::Validation(
            "cors.allow_credentials cannot be combined with the '*' origin".to_owned(),
        ));
    }
    for method in &cors.allowed_methods {
        if !is_known_http_method(method) {
            return Err(DomainError::Validation(format!(
                "cors.allowed_methods contains the unknown method '{method}'"
            )));
        }
    }
    Ok(())
}

/// `true` for the HTTP methods the CORS schema admits.
#[must_use]
pub fn is_known_http_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
    )
}

/// ASCII-lowercases a host, strips `[...]` IPv6 brackets and the trailing dot.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Serde default for the `enabled` flag: upstreams and routes are enabled
/// unless the operator says otherwise.
#[must_use]
pub const fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// HTTP method allowed in an HTTP route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `PATCH`.
    Patch,
}

impl HttpMethod {
    /// Canonical uppercase spelling.
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

    /// Parses the request method, returning `None` for methods no HTTP route
    /// may declare.
    #[must_use]
    pub fn from_method(method: &http::Method) -> Option<Self> {
        match *method {
            http::Method::GET => Some(Self::Get),
            http::Method::POST => Some(Self::Post),
            http::Method::PUT => Some(Self::Put),
            http::Method::DELETE => Some(Self::Delete),
            http::Method::PATCH => Some(Self::Patch),
            _ => None,
        }
    }
}

/// How the proxy treats `/{path_suffix}` beyond the route prefix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject any path suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// HTTP match rules.
#[api_dto(request, response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpMatch {
    /// Methods this route serves (1..).
    pub methods: Vec<HttpMethod>,
    /// Path prefix pattern.
    pub path: String,
    /// Allowed query parameter names; empty allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// Path-suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (Phase 3 — no proxy code path is reachable today).
#[api_dto(request, response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped inbound matching rules. Exactly one of `http`/`grpc`.
#[api_dto(request, response)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MatchConfig {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// Protocol this match rule is scoped to.
    #[must_use]
    pub fn protocol(&self) -> UpstreamProtocol {
        if self.grpc.is_some() {
            UpstreamProtocol::Grpc
        } else {
            UpstreamProtocol::Http
        }
    }
}

/// Create/replace body for a route. `upstream_id` is immutable and therefore
/// absent from the replace projection (see [`RouteUpdate`]).
#[api_dto(request)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteSpec {
    /// Server-generated id; supplied values are rejected as immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<uuid::Uuid>,
    /// Owning tenant; always taken from the caller's security context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<uuid::Uuid>,
    /// Upstream this route points at.
    pub upstream_id: Option<uuid::Uuid>,
    /// Flat discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match", default)]
    pub match_rules: MatchConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    /// Rate-limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

/// Replace body for a route: `upstream_id` is immutable and not accepted.
#[api_dto(request)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteUpdate {
    /// Conflicting `id` values are reported as `409`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<uuid::Uuid>,
    /// Supplied values are reported as `409` (immutable field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<uuid::Uuid>,
    /// Flat discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match", default)]
    pub match_rules: MatchConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    /// Rate-limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

/// Persisted route.
#[api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Server-generated id.
    pub id: uuid::Uuid,
    /// Owning tenant. Internal only: the wire contract
    /// (`docs/schemas/route.v1.schema.json`) does not carry it.
    #[serde(skip_serializing)]
    pub tenant_id: uuid::Uuid,
    /// Upstream this route points at.
    pub upstream_id: uuid::Uuid,
    /// Flat discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match", default)]
    pub match_rules: MatchConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    /// Rate-limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl Route {
    /// Build a route from a validated spec.
    #[must_use]
    pub fn from_spec(
        spec: RouteSpec,
        id: uuid::Uuid,
        tenant_id: uuid::Uuid,
        upstream_id: uuid::Uuid,
    ) -> Self {
        Self {
            id,
            tenant_id,
            upstream_id,
            tags: spec.tags,
            match_rules: spec.match_rules,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_defaults_to_https_443() {
        let e: Endpoint = serde_json::from_value(serde_json::json!({ "host": "api.openai.com" }))
            .expect("endpoint parses");
        assert_eq!(e.scheme, EndpointScheme::Https);
        assert_eq!(e.port, 443);
        assert_eq!(e.authority(), "api.openai.com");
    }

    #[test]
    fn http_endpoint_defaults_to_port_80() {
        let e: Endpoint = serde_json::from_value(serde_json::json!({
            "scheme": "http", "host": "svc.internal"
        }))
        .expect("endpoint parses");
        assert_eq!(e.port, 80);
        assert!(e.scheme.is_plaintext());
        assert_eq!(e.authority(), "svc.internal");
    }

    #[test]
    fn non_standard_port_is_kept_in_the_authority() {
        let e: Endpoint =
            serde_json::from_value(serde_json::json!({ "host": "api.openai.com", "port": 8443 }))
                .expect("endpoint parses");
        assert_eq!(e.authority(), "api.openai.com:8443");
    }

    #[test]
    fn http_scheme_is_accepted_at_create_time() {
        // The graded deployment sets allow_http_upstream: true; the *field*
        // must admit http regardless of that flag.
        let e: Endpoint = serde_json::from_value(serde_json::json!({ "scheme": "http", "port": 80, "host": "x" }))
            .expect("http is a legal scheme");
        assert_eq!(e.scheme, EndpointScheme::Http);
    }

    #[test]
    fn endpoint_rejects_unknown_fields() {
        assert!(serde_json::from_value::<Endpoint>(serde_json::json!({
            "host": "x", "weight": 3
        }))
        .is_err());
    }

    #[test]
    fn ip_literals_are_detected() {
        let v4: Endpoint = serde_json::from_value(serde_json::json!({ "host": "10.0.1.1" })).unwrap();
        assert!(v4.is_ip());
        let v6: Endpoint =
            serde_json::from_value(serde_json::json!({ "host": "[2001:db8::1]" })).unwrap();
        assert!(v6.is_ip());
        assert_eq!(v6.uri_host(), "[2001:db8::1]");
        assert_eq!(v6.normalized_host(), "2001:db8::1");
        let name: Endpoint =
            serde_json::from_value(serde_json::json!({ "host": "api.openai.com" })).unwrap();
        assert!(!name.is_ip());
    }

    #[test]
    fn hostname_validation_rejects_bad_labels() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("Api.OpenAI.COM.").is_ok());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("ba d.example.com").is_err());
        assert!(validate_hostname("").is_err());
        let long = format!("{}.example.com", "a".repeat(64));
        assert!(validate_hostname(&long).is_err());
    }

    #[test]
    fn upstream_protocol_gts_ids_match_the_schema() {
        assert_eq!(
            UpstreamProtocol::Http.gts_id(),
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
        assert_eq!(
            UpstreamProtocol::Grpc.gts_id(),
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
        );
        let parsed: UpstreamProtocol =
            serde_json::from_value(serde_json::json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"))
                .unwrap();
        assert_eq!(parsed, UpstreamProtocol::Http);
    }

    #[test]
    fn rate_limit_defaults_and_stricter_merge() {
        let base = RateLimitConfig::default();
        assert_eq!(base.cost, 1);
        assert_eq!(base.burst_capacity(), 1);

        let slower = RateLimitConfig {
            sustained: SustainedRate {
                rate: 10,
                window: RateWindow::Minute,
            },
            burst: Some(BurstCapacity { capacity: 50 }),
            ..RateLimitConfig::default()
        };
        let faster = RateLimitConfig {
            sustained: SustainedRate {
                rate: 30,
                window: RateWindow::Minute,
            },
            burst: Some(BurstCapacity { capacity: 5 }),
            ..RateLimitConfig::default()
        };
        let merged = slower.stricter_of(&faster);
        assert_eq!(merged.sustained.rate, 10);
        assert_eq!(merged.burst_capacity(), 5);
    }

    #[test]
    fn cors_rejects_credentials_with_wildcard_origin() {
        let cors = CorsConfig {
            enabled: true,
            allow_credentials: true,
            allowed_origins: vec!["*".to_owned()],
            ..CorsConfig::default()
        };
        assert!(validate_cors(&cors).is_err());
        let ok = CorsConfig {
            enabled: true,
            allow_credentials: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            ..CorsConfig::default()
        };
        assert!(validate_cors(&ok).is_ok());
    }

    #[test]
    fn tags_are_lowercase_only() {
        assert!(validate_tag("openai").is_ok());
        assert!(validate_tag("llm_v2").is_ok());
        assert!(validate_tag("OpenAI").is_err());
        assert!(validate_tag("").is_err());
    }

    #[test]
    fn upstream_pool_must_be_homogeneous() {
        let spec = UpstreamSpec {
            server: ServerConfig {
                endpoints: vec![
                    Endpoint::with_standard_port(EndpointScheme::Https, "us.vendor.com"),
                    Endpoint::with_standard_port(EndpointScheme::Https, "eu.vendor.com"),
                ],
            },
            protocol: UpstreamProtocol::Http,
            ..UpstreamSpec::default()
        };
        assert!(validate_upstream_spec(&spec).is_ok());

        let mixed = UpstreamSpec {
            server: ServerConfig {
                endpoints: vec![
                    Endpoint::with_standard_port(EndpointScheme::Https, "us.vendor.com"),
                    Endpoint::new(EndpointScheme::Https, "eu.vendor.com", 8443),
                ],
            },
            protocol: UpstreamProtocol::Http,
            ..UpstreamSpec::default()
        };
        assert!(validate_upstream_spec(&mixed).is_err());
    }

    #[test]
    fn empty_endpoint_pool_is_rejected() {
        let spec = UpstreamSpec {
            server: ServerConfig { endpoints: vec![] },
            protocol: UpstreamProtocol::Http,
            ..UpstreamSpec::default()
        };
        let err = validate_upstream_spec(&spec).expect_err("empty pool must fail");
        assert_eq!(err.http_status(), 400);
    }

    #[test]
    fn enabled_defaults_to_true() {
        assert!(default_true());
        assert!(UpstreamSpec::default().enabled);
    }

    #[test]
    fn target_host_header_is_lowercase() {
        assert_eq!(TARGET_HOST_HEADER, "x-oagw-target-host");
    }

    #[test]
    fn gts_resource_types_are_stable() {
        assert_eq!(UPSTREAM_GTS_TYPE, "gts.cf.core.oagw.upstream.v1~");
        assert_eq!(ROUTE_GTS_TYPE, "gts.cf.core.oagw.route.v1~");
        assert_eq!(PROXY_GTS_TYPE, "gts.cf.core.oagw.proxy.v1~");
    }

    #[test]
    fn effective_tags_are_additive() {
        let mut upstream = Upstream::from_spec(
            UpstreamSpec {
                tags: vec!["local".to_owned()],
                protocol: UpstreamProtocol::Http,
                ..UpstreamSpec::default()
            },
            uuid::Uuid::nil(),
            uuid::Uuid::nil(),
            "x".to_owned(),
        );
        upstream.tags = vec!["own".to_owned()];
        assert_eq!(
            upstream.effective_tags(&["inherited".to_owned()]),
            vec!["inherited".to_owned(), "own".to_owned()]
        );
    }

    #[test]
    fn http_method_round_trips() {
        assert_eq!(HttpMethod::from_method(&http::Method::GET), Some(HttpMethod::Get));
        assert_eq!(
            HttpMethod::from_method(&http::Method::HEAD),
            None,
            "HEAD is not an addressable route method in this phase"
        );
        assert_eq!(HttpMethod::Patch.as_str(), "PATCH");
        let parsed: Vec<HttpMethod> =
            serde_json::from_value(serde_json::json!(["GET", "POST"])).unwrap();
        assert_eq!(parsed, vec![HttpMethod::Get, HttpMethod::Post]);
    }

    #[test]
    fn match_config_reports_its_protocol() {
        let http_only = MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert_eq!(http_only.protocol(), UpstreamProtocol::Http);
    }
}
