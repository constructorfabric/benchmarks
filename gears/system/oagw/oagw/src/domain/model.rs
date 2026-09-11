//! The OAGW configuration model.
//!
//! Types mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`; validation reproduces the constraints
//! those schemas state plus the alias rules in `DESIGN.md`.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::error::OagwError;
use crate::config::OagwConfig;
use crate::gts;

// The management API exchanges these types directly, so they are registered as
// API DTOs even though they carry their own serde attributes (which the
// `#[toolkit_macros::api_dto]` derive would clobber with a blanket
// `rename_all`).
impl toolkit::api::api_dto::RequestApiDto for Upstream {}
impl toolkit::api::api_dto::ResponseApiDto for Upstream {}
impl toolkit::api::api_dto::RequestApiDto for Route {}
impl toolkit::api::api_dto::ResponseApiDto for Route {}
impl toolkit::api::api_dto::RequestApiDto for Plugin {}
impl toolkit::api::api_dto::ResponseApiDto for Plugin {}

/// Sharing mode for hierarchical configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Visible only to the owning tenant.
    #[default]
    Private,
    /// Visible to descendants, which may override it.
    Inherit,
    /// Visible to descendants, which may not override it.
    Enforce,
}

impl SharingMode {
    /// Whether this mode exposes the setting to descendants.
    #[must_use]
    pub fn is_visible_to_descendants(self) -> bool {
        matches!(self, Self::Inherit | Self::Enforce)
    }

    /// Whether descendants are allowed to override the setting.
    #[must_use]
    pub fn allows_override(self) -> bool {
        matches!(self, Self::Inherit)
    }
}

/// Upstream endpoint scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP. Accepted only when `allow_http_upstream` is set.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl Scheme {
    /// Parses a scheme literal.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a scheme outside the accepted set.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw.to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            "wss" => Ok(Self::Wss),
            "wt" => Ok(Self::Wt),
            "grpc" => Ok(Self::Grpc),
            other => Err(OagwError::validation(format!(
                "invalid endpoint scheme {other:?}; expected one of https, wss, wt, grpc"
            ))),
        }
    }

    /// Whether the scheme speaks HTTP (all supported transports do).
    #[must_use]
    pub fn is_http(self) -> bool {
        matches!(
            self,
            Self::Http | Self::Https | Self::Wss | Self::Wt | Self::Grpc
        )
    }

    /// Whether the scheme upgrades to a WebSocket.
    #[must_use]
    pub fn is_websocket(self) -> bool {
        self == Self::Wss
    }

    /// Whether the scheme is plaintext.
    #[must_use]
    pub fn is_plaintext(self) -> bool {
        self == Self::Http
    }

    /// The scheme's standard port.
    #[must_use]
    pub fn standard_port(self) -> u16 {
        if self == Self::Http { 80 } else { 443 }
    }

    /// Default port used when the endpoint does not name one.
    #[must_use]
    pub fn default_port(self) -> u16 {
        self.standard_port()
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// URI scheme.
    #[serde(default = "default_scheme")]
    pub scheme: Scheme,
    /// Hostname, IPv4 or IPv6 address.
    pub host: String,
    /// TCP port (default 443, 80 for `http`).
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_scheme() -> Scheme {
    Scheme::Https
}

impl Endpoint {
    /// Effective port, applying the scheme default when unset.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// `host` as sent in the `Host`/`:authority` pseudo-header.
    #[must_use]
    pub fn host_header(&self) -> String {
        match (self.host.contains(':'), self.effective_port()) {
            (true, port) => format!(
                "[{}]:{}",
                self.host.trim_start_matches('[').trim_end_matches(']'),
                port
            ),
            (false, port) => format!("{}:{}", self.host, port),
        }
    }

    /// `host` alone, used for alias derivation and target-host matching.
    #[must_use]
    pub fn bare_host(&self) -> String {
        self.host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase()
    }

    /// Parses the host into an IP address when it is one.
    #[must_use]
    pub fn ip(&self) -> Option<IpAddr> {
        self.bare_host().parse().ok()
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let host = &self.host;
        let port = self.effective_port();
        if host.contains(':') && !host.starts_with('[') {
            write!(f, "[{host}]:{port}")
        } else {
            write!(f, "{host}:{port}")
        }
    }
}

/// `server` stanza.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Server {
    /// Declared endpoints (one or more).
    pub endpoints: Vec<Endpoint>,
}

/// `auth` stanza.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Auth {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: Option<String>,
    /// Sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration (opaque to the gateway core).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: serde_json::Map<String, Value>,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            plugin_type: Some(gts::auth_plugin::NOOP.to_owned()),
            sharing: SharingMode::default(),
            config: Default::default(),
        }
    }
}

/// Request header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Headers to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default, skip_serializing_if = "is_passthrough_none")]
    pub passthrough: Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

fn is_passthrough_none(p: &Passthrough) -> bool {
    *p == Passthrough::None
}

/// Inbound header forwarding policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward no inbound headers (default).
    #[default]
    None,
    /// Forward only the allowlisted headers.
    Allowlist,
    /// Forward every inbound header.
    All,
}

/// Response header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResponseHeaderRules {
    /// Headers to set (overwrite).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Headers to strip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeaderRules {
    /// Inbound → upstream.
    #[serde(default, skip_serializing_if = "RequestHeaderRules::is_empty")]
    pub request: RequestHeaderRules,
    /// Upstream → client.
    #[serde(default, skip_serializing_if = "ResponseHeaderRules::is_empty")]
    pub response: ResponseHeaderRules,
}

impl RequestHeaderRules {
    /// Whether no rule is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.passthrough == Passthrough::None
            && self.passthrough_allowlist.is_empty()
    }
}

impl ResponseHeaderRules {
    /// Whether no rule is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.add.is_empty() && self.remove.is_empty()
    }
}

/// Plugin chain binding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginChain {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin identifiers (built-in GTS ids or custom plugin UUIDs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// Rate limit window.
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
    /// Human-readable window label used in problem details.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }

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

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (bursts allowed).
    #[default]
    TokenBucket,
    /// Sliding window (no boundary bursts).
    SlidingWindow,
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One counter per process.
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

/// Behavior when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    /// Queue the request until a token is available.
    Queue,
    /// Degrade the response.
    Degrade,
}

/// Sustained rate component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateSustained {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window length (default `second`).
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateBurst {
    /// Bucket capacity.
    pub capacity: u32,
}

/// Rate limiting configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimit {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm (default `token_bucket`).
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: RateSustained,
    /// Burst capacity (default `sustained.rate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<RateBurst>,
    /// Counter scope (default `tenant`).
    #[serde(default)]
    pub scope: RateScope,
    /// Strategy on exhaustion (default `reject`).
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request (default 1).
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Emit `X-RateLimit-*` headers (default true).
    #[serde(default = "default_true", skip_serializing_if = "is_false")]
    pub response_headers: bool,
}

fn default_true() -> bool {
    true
}

fn is_false(v: &bool) -> bool {
    !*v
}

fn default_cost() -> u32 {
    1
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: RateSustained {
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

impl RateLimit {
    /// Effective bucket capacity.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst
            .as_ref()
            .map_or(self.sustained.rate, |b| b.capacity)
    }

    /// Tokens replenished per second.
    #[must_use]
    pub fn rate_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.seconds() as f64
    }
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Cors {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS.
    pub enabled: bool,
    /// Allowed origins; `["*"]` for any origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
    /// Allowed methods (default `[GET, POST]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_methods: Option<Vec<String>>,
    /// Headers exposed to the browser.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_credentials: bool,
}

impl Default for Cors {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: None,
            allowed_methods: None,
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl Cors {
    /// Default `allowed_methods`.
    #[must_use]
    pub fn effective_methods(&self) -> Vec<String> {
        self.allowed_methods
            .clone()
            .unwrap_or_else(|| vec!["GET".to_owned(), "POST".to_owned()])
    }

    /// Effective `allowed_origins`.
    #[must_use]
    pub fn effective_origins(&self) -> Vec<String> {
        self.allowed_origins.clone().unwrap_or_default()
    }
}

/// An upstream service configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Upstream {
    /// System-generated identifier.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
    /// Owning tenant (server-assigned, never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub tenant_id: Option<Uuid>,
    /// Enabled flag.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Server endpoints.
    pub server: Server,
    /// Protocol identifier.
    pub protocol: String,
    /// Auth plugin binding.
    #[serde(default)]
    pub auth: Auth,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeaderRules,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginChain,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
    /// Creation timestamp (never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub created_at: Option<String>,
    /// Last modification timestamp (never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub updated_at: Option<String>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: None,
            tenant_id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: Server {
                endpoints: Vec::new(),
            },
            protocol: gts::PROTOCOL_HTTP.to_owned(),
            auth: Auth::default(),
            headers: HeaderRules::default(),
            plugins: PluginChain::default(),
            rate_limit: None,
            cors: None,
            created_at: None,
            updated_at: None,
        }
    }
}

/// A route configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Route {
    /// System-generated identifier.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
    /// Owning tenant (server-assigned, never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub tenant_id: Option<Uuid>,
    /// Owning upstream (immutable).
    pub upstream_id: String,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Whether the route accepts traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Route-level CORS overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginChain,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Creation timestamp (never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub created_at: Option<String>,
    /// Last modification timestamp (never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub updated_at: Option<String>,
}

impl Route {
    /// `upstream_id` without the GTS type prefix.
    #[must_use]
    pub fn upstream_uuid(&self) -> &str {
        gts::unqualify(gts::UPSTREAM_TYPE, &self.upstream_id)
    }
}

/// Protocol-scoped inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub struct RouteMatch {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct HttpMatch {
    /// HTTP methods matched by this route.
    pub methods: Vec<String>,
    /// Path pattern.
    pub path: String,
    /// Query parameters allowed through (empty allowlist rejects all).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated (default `append`).
    #[serde(default, skip_serializing_if = "is_suffix_append")]
    pub path_suffix_mode: PathSuffixMode,
}

fn is_suffix_append(v: &PathSuffixMode) -> bool {
    *v == PathSuffixMode::Append
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Path suffix handling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// A custom (tenant-defined) plugin definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Plugin {
    /// System-generated identifier.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub id: Option<String>,
    /// Owning tenant (server-assigned, never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub tenant_id: Option<Uuid>,
    /// Human-readable name (unique per tenant).
    pub name: String,
    /// Description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// One of `auth`, `guard`, `transform`.
    pub plugin_type: String,
    /// JSON schema describing the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<Value>,
    /// Starlark source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Execution phases (e.g. `["on_request"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    /// Creation timestamp (never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub created_at: Option<String>,
    /// Last modification timestamp (never serialized).
    #[serde(skip_serializing, default)]
    #[schema(ignore)]
    pub updated_at: Option<String>,
}

impl Plugin {
    /// GTS type prefix for this plugin's kind.
    #[must_use]
    pub fn type_prefix(&self) -> &'static str {
        match self.plugin_type.as_str() {
            "auth" => "gts.cf.core.oagw.auth_plugin.v1~",
            "guard" => "gts.cf.core.oagw.guard_plugin.v1~",
            _ => "gts.cf.core.oagw.transform_plugin.v1~",
        }
    }
}

/// Validates a tag against `^[a-z0-9_-]+$`.
fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Validates an alias against `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty() || alias.len() > 253 {
        return false;
    }
    let bytes = alias.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    if !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit() {
        return false;
    }
    bytes.iter().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b':' || *b == b'-'
    })
}

/// Validates an RFC 1123 hostname (a trailing dot is tolerated and stripped).
#[must_use]
pub fn normalize_hostname(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return None;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return None;
        }
    }
    Some(host)
}

/// Whether the host is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

impl Upstream {
    /// Normalized endpoints (host lowercased, trailing dot stripped).
    #[must_use]
    pub fn normalized_endpoints(&self) -> Vec<Endpoint> {
        self.server
            .endpoints
            .iter()
            .map(|e| {
                let host = if is_ip_literal(&e.host) {
                    e.bare_host()
                } else {
                    normalize_hostname(&e.host).unwrap_or_else(|| e.bare_host())
                };
                Endpoint {
                    scheme: e.scheme,
                    host,
                    port: e.port,
                }
            })
            .collect()
    }

    /// Validates the upstream against the schema constraints and alias rules.
    ///
    /// # Errors
    ///
    /// Returns a 400 problem for every violation described in the schema.
    pub fn validate(&self, cfg: &OagwConfig) -> Result<(), OagwError> {
        if self.server.endpoints.is_empty() {
            return Err(OagwError::validation(
                "server.endpoints must contain at least one endpoint",
            ));
        }
        for endpoint in &self.server.endpoints {
            if endpoint.scheme.is_plaintext() && !cfg.allow_http_upstream {
                return Err(OagwError::validation(
                    "plaintext http upstreams are not permitted by this deployment",
                ));
            }
            if is_ip_literal(&endpoint.host) {
                if endpoint.bare_host().parse::<IpAddr>().is_err() {
                    return Err(OagwError::validation(format!(
                        "invalid IP address in endpoint host {:?}",
                        endpoint.host
                    )));
                }
            } else if normalize_hostname(&endpoint.host).is_none() {
                return Err(OagwError::validation(format!(
                    "invalid hostname in endpoint host {:?}",
                    endpoint.host
                )));
            }
        }

        match self.protocol.as_str() {
            gts::PROTOCOL_HTTP | gts::PROTOCOL_GRPC => {}
            other => {
                return Err(OagwError::validation(format!(
                    "unsupported protocol {other:?}; expected {http:?} or {grpc:?}",
                    http = gts::PROTOCOL_HTTP,
                    grpc = gts::PROTOCOL_GRPC
                )));
            }
        }

        for tag in &self.tags {
            if !is_valid_tag(tag) {
                return Err(OagwError::validation(format!(
                    "invalid tag {tag:?}; must match ^[a-z0-9_-]+$"
                )));
            }
        }

        if let Some(alias) = &self.alias {
            let normalized = alias.trim().trim_end_matches('.').to_ascii_lowercase();
            if !is_valid_alias(&normalized) {
                return Err(OagwError::validation(format!(
                    "invalid alias {alias:?}; must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
                )));
            }
        }

        if let Some(rate_limit) = &self.rate_limit {
            rate_limit.validate("upstream.rate_limit")?;
        }
        if let Some(cors) = &self.cors {
            cors.validate("upstream.cors")?;
        }

        if let Some(cors) = &self.cors {
            cors.validate("upstream.cors")?;
        }

        Ok(())
    }
}

impl RateLimit {
    /// Validates the rate limit stanza.
    ///
    /// # Errors
    ///
    /// Returns a 400 problem when a component is out of range.
    pub fn validate(&self, field: &str) -> Result<(), OagwError> {
        if self.sustained.rate == 0 {
            return Err(OagwError::validation(format!(
                "{field}.sustained.rate must be at least 1"
            )));
        }
        if let Some(burst) = &self.burst
            && burst.capacity == 0
        {
            return Err(OagwError::validation(format!(
                "{field}.burst.capacity must be at least 1"
            )));
        }
        if self.cost == 0 {
            return Err(OagwError::validation(format!(
                "{field}.cost must be at least 1"
            )));
        }
        Ok(())
    }
}

impl Cors {
    /// Validates the CORS stanza.
    ///
    /// # Errors
    ///
    /// Returns a 400 problem when credentials are combined with a wildcard
    /// origin.
    pub fn validate(&self, field: &str) -> Result<(), OagwError> {
        if self.allow_credentials && self.effective_origins().iter().any(|o| o == "*") {
            return Err(OagwError::validation(format!(
                "{field}.allow_credentials may not be combined with allowed_origins [\"*\"]"
            )));
        }
        Ok(())
    }
}

impl Route {
    /// Validates the route.
    ///
    /// # Errors
    ///
    /// Returns a 400 problem describing the first violation found.
    pub fn validate(&self) -> Result<(), OagwError> {
        let has_http = self.match_.http.is_some();
        let has_grpc = self.match_.grpc.is_some();
        if has_http == has_grpc {
            return Err(OagwError::validation(
                "route.match requires exactly one of {http|grpc}",
            ));
        }
        if let Some(http) = &self.match_.http {
            if http.methods.is_empty() {
                return Err(OagwError::validation(
                    "route.match.http.methods must contain at least one method",
                ));
            }
            for method in &http.methods {
                if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "DELETE" | "PATCH") {
                    return Err(OagwError::validation(format!(
                        "route.match.http.methods contains unsupported method {method:?}"
                    )));
                }
            }
            if http.path.is_empty() {
                return Err(OagwError::validation(
                    "route.match.http.path must not be empty",
                ));
            }
            if !http.path.starts_with('/') {
                return Err(OagwError::validation(
                    "route.match.http.path must begin with '/'",
                ));
            }
        }
        if let Some(grpc) = &self.match_.grpc
            && (grpc.service.is_empty() || grpc.method.is_empty())
        {
            return Err(OagwError::validation(
                "route.match.grpc requires both service and method",
            ));
        }
        for tag in &self.tags {
            if !is_valid_tag(tag) {
                return Err(OagwError::validation(format!(
                    "invalid tag {tag:?}; must match ^[a-z0-9_-]+$"
                )));
            }
        }
        if let Some(rate_limit) = &self.rate_limit {
            rate_limit.validate("route.rate_limit")?;
        }
        if let Some(cors) = &self.cors {
            cors.validate("route.cors")?;
        }
        Ok(())
    }
}
