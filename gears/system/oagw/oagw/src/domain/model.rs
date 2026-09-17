//! The OAGW control-plane domain model.
//!
//! Types here are transport-agnostic: no `axum`, no `http`, no storage. The
//! field set, the defaults, and the validation rules mirror
//! `docs/schemas/upstream.v1.schema.json` and `docs/schemas/route.v1.schema.json`.
//!
//! Wire faithfulness of the serde representation matters: the `$filter` /
//! `$orderby` engine (`crate::domain::query`) evaluates against the *domain*
//! serialization, so field names here are the API field names.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::reason;

/// Alias syntax: lower-case, 3..=253 chars, `[a-z0-9]` first and last, and
/// `-`, `:`, `.` allowed in the middle (`docs/schemas/upstream.v1.schema.json`).
pub const ALIAS_PATTERN: &str = "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$";
/// Tag syntax: lower-case `[a-z0-9_-]+`.
pub const TAG_PATTERN: &str = "^[a-z0-9_-]+$";
/// Longest accepted alias.
pub const ALIAS_MAX_LEN: usize = 253;

/// Per-tenant sharing mode for an embedded configuration block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// The block applies only to the owning tenant.
    #[default]
    Private,
    /// Descendant tenants inherit the block.
    Inherit,
    /// Descendant tenants may reference but not override the block.
    Enforce,
}

/// Wire transport of a pooled endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP/1.1 or h2c. Legal as an `endpoint.scheme` value;
    /// whether plaintext is actually produced is governed at runtime by
    /// `oagw.config.allow_http_upstream`.
    Http,
    /// TLS-terminated HTTP.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport over TLS.
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl EndpointScheme {
    /// Port used when the endpoint carries no explicit port.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// The scheme is a plaintext one (only `http` today).
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http)
    }
}

/// A single pooled upstream endpoint: `scheme://host:port`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Wire scheme of the pooled endpoint.
    pub scheme: EndpointScheme,
    /// DNS name, IPv4 or IPv6 literal (never a URL, never a path).
    pub host: String,
    /// Port; defaults to the scheme's standard port.
    pub port: u16,
}

impl Endpoint {
    /// Build and validate one endpoint. `host` is normalized to lower case.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] when the host is not a valid DNS name
    /// or IP literal, or the port is out of range.
    pub fn new(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Result<Self, DomainError> {
        let normalized = normalize_host(host)?;
        Ok(Self {
            scheme,
            host: normalized,
            port: port.unwrap_or_else(|| scheme.standard_port()),
        })
    }

    /// `true` when the host is an IPv4 or IPv6 literal.
    #[must_use]
    pub fn is_ip_literal(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }

    /// Re-validate an endpoint that came from storage.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when stored data no longer validates.
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_host(&self.host)?;
        if self.port == 0 {
            return Err(DomainError::field(
                "port",
                reason::PORT_OUT_OF_RANGE,
                "port must be between 1 and 65535",
            ));
        }
        Ok(())
    }
}

/// Normalize a hostname: trim, lower-case, strip a trailing dot, and verify
/// it is either an IP literal or a syntactically valid DNS name.
///
/// # Errors
///
/// [`DomainError::FieldViolation`] with [`reason::HOST_INVALID`].
pub fn normalize_host(host: &str) -> Result<String, DomainError> {
    let trimmed = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if trimmed.is_empty() {
        return Err(DomainError::field(
            "host",
            reason::HOST_INVALID,
            "host must not be empty",
        ));
    }
    validate_host(&trimmed)?;
    Ok(trimmed)
}

/// Validate an already-lower-cased host value.
///
/// # Errors
///
/// [`DomainError::FieldViolation`] with [`reason::HOST_INVALID`].
pub fn validate_host(host: &str) -> Result<(), DomainError> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    if host.contains(':') {
        return Err(DomainError::field(
            "host",
            reason::HOST_INVALID,
            format!("'{host}' is not a valid IPv6 literal"),
        ));
    }
    let labels: Vec<&str> = host.split('.').collect();
    if host.len() > 253 || labels.len() < 2 {
        return Err(DomainError::field(
            "host",
            reason::HOST_INVALID,
            format!("'{host}' is not a valid DNS name"),
        ));
    }
    for label in labels {
        if label.is_empty() || label.len() > 63 {
            return Err(DomainError::field(
                "host",
                reason::HOST_INVALID,
                format!("'{host}' has an invalid DNS label"),
            ));
        }
        let ok = label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !ok {
            return Err(DomainError::field(
                "host",
                reason::HOST_INVALID,
                format!("'{host}' has an invalid DNS label '{label}'"),
            ));
        }
    }
    Ok(())
}

/// Endpoint pool of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    /// At least one endpoint is always required.
    pub endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// Validate the pool. The pool may never be empty: a disabled upstream
    /// still keeps its endpoints so it can be re-enabled. Every endpoint of
    /// a pool must agree on scheme and port (`DESIGN.md` §3.3
    /// "Multi-Endpoint Load Balancing"), since a pool is load-balanced, not
    /// fanned out across transports.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] with [`reason::ENDPOINTS_EMPTY`].
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.endpoints.is_empty() {
            return Err(DomainError::field(
                "server.endpoints",
                reason::ENDPOINTS_EMPTY,
                "at least one endpoint is required",
            ));
        }
        let (scheme, port) = (self.endpoints[0].scheme, self.endpoints[0].port);
        for endpoint in &self.endpoints {
            endpoint.validate()?;
            if endpoint.scheme != scheme || endpoint.port != port {
                return Err(DomainError::field(
                    "server.endpoints",
                    "endpoints.heterogeneous",
                    "all endpoints of an upstream must share the same scheme and port",
                ));
            }
        }
        Ok(())
    }
}

/// Wire protocol an upstream speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP/1.1 or HTTP/2.
    Http,
    /// gRPC / gRPC-Web.
    Grpc,
}

impl Protocol {
    /// Anonymous GTS id of the HTTP protocol definition.
    pub const HTTP_ID: &'static str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    /// Anonymous GTS id of the gRPC protocol definition.
    pub const GRPC_ID: &'static str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

    /// The protocol's anonymous GTS id (the wire form).
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => Self::HTTP_ID,
            Self::Grpc => Self::GRPC_ID,
        }
    }

    /// Parse a protocol GTS id.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] with [`reason::PROTOCOL_UNKNOWN`].
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        match raw {
            Self::HTTP_ID => Ok(Self::Http),
            Self::GRPC_ID => Ok(Self::Grpc),
            other => Err(DomainError::field(
                "protocol",
                reason::PROTOCOL_UNKNOWN,
                format!("'{other}' is not a known oagw protocol type"),
            )),
        }
    }
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.gts_id())
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Authentication plugin attached to an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Full GTS id of the auth plugin (built-in catalog id or custom plugin).
    pub plugin_type: String,
    /// Resolved plugin UUID when the reference is a custom plugin row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// Sharing of the auth configuration with descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Opaque plugin configuration (must be a JSON object when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl AuthConfig {
    /// Validate shape only; bindability is decided by
    /// [`crate::domain::services::management::built_in_plugin`].
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] when the reference or config is empty.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.plugin_type.trim().is_empty() {
            return Err(DomainError::field(
                "auth.type",
                reason::MISSING,
                "auth.type must not be empty",
            ));
        }
        validate_config_object("auth.config", &self.config)?;
        Ok(())
    }
}

/// Header manipulation rules applied before a request leaves the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HeadersConfig {
    /// Request headers addressed to the upstream.
    #[serde(default)]
    pub request: RequestHeaders,
    /// Response headers addressed to the caller.
    #[serde(default)]
    pub response: ResponseHeaders,
}

/// `headers.request` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// Headers set (replaced) on the forwarded request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<HeaderSetting>,
    /// Headers appended to the forwarded request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<HeaderSetting>,
    /// Header names removed from the forwarded request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// How the caller's own request headers are forwarded.
    #[serde(default)]
    pub passthrough: HeaderPassthrough,
    /// Allowlist used when `passthrough` is [`HeaderPassthrough::Allowlist`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// `headers.response` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// Headers set (replaced) on the response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<HeaderSetting>,
    /// Headers appended to the response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<HeaderSetting>,
    /// Header names removed from the response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// A single header assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderSetting {
    /// Header name (US-ASCII token).
    pub name: String,
    /// Header value template.
    pub value: String,
}

/// How much of the caller's original request header set is forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPassthrough {
    /// Forward nothing but the gateway's own headers.
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward the caller's headers untouched.
    #[default]
    All,
}

impl HeadersConfig {
    /// Validate header names and passthrough consistency.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] on an empty header name, or when
    /// `passthrough: allowlist` is combined with an empty allowlist.
    pub fn validate(&self) -> Result<(), DomainError> {
        for setting in self
            .request
            .set
            .iter()
            .chain(&self.request.add)
            .chain(&self.response.set)
            .chain(&self.response.add)
        {
            validate_header_name(&setting.name)?;
        }
        for name in self.request.remove.iter().chain(&self.response.remove) {
            validate_header_name(name)?;
        }
        if self.request.passthrough == HeaderPassthrough::Allowlist
            && self.request.passthrough_allowlist.is_empty()
        {
            return Err(DomainError::field(
                "headers.request.passthrough_allowlist",
                reason::MISSING,
                "passthrough_allowlist is required when passthrough is 'allowlist'",
            ));
        }
        Ok(())
    }
}

/// Validate a US-ASCII header token.
///
/// # Errors
///
/// [`DomainError::FieldViolation`] when the name is empty or not a token.
fn validate_header_name(name: &str) -> Result<(), DomainError> {
    if name.is_empty() {
        return Err(DomainError::field(
            "headers",
            reason::MISSING,
            "header name must not be empty",
        ));
    }
    let ok = name.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    });
    if ok {
        Ok(())
    } else {
        Err(DomainError::field(
            "headers",
            "headers.invalid_name",
            format!("'{name}' is not a valid header name"),
        ))
    }
}

/// An ordered plugin binding on an upstream or route chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginBinding {
    /// Full GTS id of the plugin (built-in catalog id, or a custom plugin's
    /// anonymous id `gts.cf.core.oagw.plugin.v1~{uuid}`).
    pub plugin_ref: String,
    /// Resolved plugin UUID when the reference is a custom plugin row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// Plugin configuration object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl PluginBinding {
    /// Build a binding for a reference that needs no resolution.
    #[must_use]
    pub fn bare(plugin_ref: impl Into<String>) -> Self {
        Self {
            plugin_ref: plugin_ref.into(),
            plugin_uuid: None,
            config: None,
        }
    }
}

/// `plugins` block of an upstream or route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct PluginsConfig {
    /// Sharing of the plugin chain with descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered chain; the gateway executes it top to bottom.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Rate-limit window unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// One second.
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
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained throughput half of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Requests allowed per `window`.
    pub rate: u32,
    /// Averaging window.
    pub window: RateWindow,
}

/// Burst allowance on top of the sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstConfig {
    /// Bucket capacity in requests.
    pub capacity: u32,
}

/// Token bucket or sliding window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Classic token bucket.
    #[default]
    TokenBucket,
    /// Sliding window counter.
    SlidingWindow,
}

/// Who the limiter keys on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket for the whole gear.
    #[default]
    Global,
    /// One bucket per calling tenant.
    Tenant,
    /// One bucket per authenticated user.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per matched route.
    Route,
}

/// What happens when the bucket is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Answer `429` immediately.
    #[default]
    Reject,
    /// Hold the request until a token frees up.
    Queue,
    /// Forward the request in a degraded mode.
    Degrade,
}

/// Rate limit configuration. The limiter itself is a data-plane concern and
/// is *not* part of this crate's management dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing of the limit with descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Limiting algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained throughput.
    pub sustained: SustainedRate,
    /// Burst allowance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    /// Bucket key.
    #[serde(default)]
    pub scope: RateScope,
    /// Saturation behaviour.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Cost of one matched request in tokens.
    #[serde(default = "default_cost")]
    pub cost: u32,
}

fn default_cost() -> u32 {
    1
}

impl RateLimitConfig {
    /// Validate the numeric ranges.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] when `rate`, `capacity` or `cost` is 0.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.sustained.rate == 0 {
            return Err(DomainError::field(
                "rate_limit.sustained.rate",
                "rate_limit.rate.invalid",
                "sustained rate must be greater than 0",
            ));
        }
        if let Some(burst) = &self.burst
            && burst.capacity == 0
        {
            return Err(DomainError::field(
                "rate_limit.burst.capacity",
                "rate_limit.capacity.invalid",
                "burst capacity must be greater than 0",
            ));
        }
        if self.cost == 0 {
            return Err(DomainError::field(
                "rate_limit.cost",
                "rate_limit.cost.invalid",
                "cost must be greater than 0",
            ));
        }
        Ok(())
    }
}

/// CORS configuration. Enforcement is a data-plane concern and is *not* part
/// of this crate's management dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing of the CORS policy with descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether the policy is active.
    pub enabled: bool,
    /// `*` or absolute URIs.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Methods the pre-flight may advertise.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials may cross the origin boundary.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
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

impl CorsConfig {
    /// Validate origin syntax and the credentials / wildcard rule.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] on a malformed origin or on
    /// `allow_credentials: true` combined with an `allowed_origins` wildcard.
    pub fn validate(&self) -> Result<(), DomainError> {
        for origin in &self.allowed_origins {
            if origin == "*" {
                continue;
            }
            let ok = origin.contains("://")
                && !origin.contains(' ')
                && !origin.starts_with("://")
                && !origin.ends_with('/')
                && origin.chars().all(|c| c.is_ascii() && !c.is_control());
            if !ok {
                return Err(DomainError::field(
                    "cors.allowed_origins",
                    "cors.origin.invalid",
                    format!("'{origin}' is neither '*' nor an absolute URI"),
                ));
            }
        }
        if self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*") {
            return Err(DomainError::field(
                "cors.allow_credentials",
                reason::CORS_CREDENTIALS_WILDCARD,
                "allow_credentials must not be combined with a wildcard origin",
            ));
        }
        Ok(())
    }
}

/// A management-plane upstream: a pooled, aliased backend plus the chain that
/// shapes traffic towards it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// Server-generated UUID (also addressable as an anonymous GTS id).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Tenant-unique, URL-safe name; may be derived from the endpoints.
    pub alias: String,
    /// Disabled upstreams are not matched by the proxy.
    pub enabled: bool,
    /// Free-form lower-case labels.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Wire protocol towards the backend.
    pub protocol: Protocol,
    /// Optional authentication plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Optional header rewriting rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Optional plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Optional rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Optional CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation instant, unix epoch milliseconds.
    pub created_at: u64,
    /// Last mutation instant, unix epoch milliseconds.
    pub updated_at: u64,
}

impl Upstream {
    /// Validate the upstream's own invariants (not cross-resource ones).
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`].
    pub fn validate(&self) -> Result<(), DomainError> {
        validate_alias(&self.alias)?;
        validate_tags(&self.tags)?;
        self.server.validate()?;
        if let Some(auth) = &self.auth {
            auth.validate()?;
        }
        if let Some(headers) = &self.headers {
            headers.validate()?;
        }
        if let Some(plugins) = &self.plugins {
            for binding in &plugins.items {
                if binding.plugin_ref.trim().is_empty() {
                    return Err(DomainError::field(
                        "plugins.items",
                        reason::PLUGIN_UNKNOWN,
                        "plugin reference must not be empty",
                    ));
                }
                validate_config_object("plugins.items.config", &binding.config)?;
            }
        }
        if let Some(rate_limit) = &self.rate_limit {
            rate_limit.validate()?;
        }
        if let Some(cors) = &self.cors {
            cors.validate()?;
        }
        Ok(())
    }

    /// The upstream's anonymous GTS id.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts::gts_id(crate::domain::gts::UPSTREAM_TYPE, &self.id)
    }
}

/// What an HTTP route matches on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// HTTP methods; at least one.
    pub methods: Vec<String>,
    /// Literal or wildcard path pattern (`/v1/*`).
    pub path: String,
    /// Query parameter names that must (or must not) be present.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How a longer request path is handled.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// `match.path_suffix_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Longer request paths are not matched.
    #[default]
    Disabled,
    /// The suffix after the pattern is appended to the upstream path.
    Append,
}

/// What a gRPC route matches on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// Method name, or `*` for the whole service.
    pub method: String,
}

/// Route match discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMatcher {
    /// HTTP matching rules.
    Http(HttpMatch),
    /// gRPC matching rules.
    Grpc(GrpcMatch),
}

impl RouteMatcher {
    /// Validate the matcher shape.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] when methods, path, service or method
    /// are empty.
    pub fn validate(&self) -> Result<(), DomainError> {
        match self {
            Self::Http(http) => {
                if http.methods.is_empty() {
                    return Err(DomainError::field(
                        "match.methods",
                        reason::MISSING,
                        "at least one HTTP method is required",
                    ));
                }
                for method in &http.methods {
                    if !matches!(
                        method.as_str(),
                        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
                    ) {
                        return Err(DomainError::field(
                            "match.methods",
                            "match.method.unsupported",
                            format!("'{method}' is not a routable HTTP method"),
                        ));
                    }
                }
                if http.path.is_empty() {
                    return Err(DomainError::field(
                        "match.path",
                        reason::MISSING,
                        "match.path must not be empty",
                    ));
                }
                if !http.path.starts_with('/') {
                    return Err(DomainError::field(
                        "match.path",
                        "match.path.invalid",
                        format!("'{}' must start with '/'", http.path),
                    ));
                }
            }
            Self::Grpc(grpc) => {
                if grpc.service.is_empty() {
                    return Err(DomainError::field(
                        "match.service",
                        reason::MISSING,
                        "match.service must not be empty",
                    ));
                }
                if grpc.method.is_empty() {
                    return Err(DomainError::field(
                        "match.method",
                        reason::MISSING,
                        "match.method must not be empty",
                    ));
                }
            }
        }
        Ok(())
    }

    /// The canonical match key used for the per-upstream uniqueness
    /// invariant: `<METHODS>|<path-or-service>|<method>`.
    #[must_use]
    pub fn match_key(&self) -> String {
        match self {
            Self::Http(http) => {
                let mut methods = http.methods.clone();
                methods.sort_unstable();
                format!("{}|{}", methods.join(","), http.path)
            }
            Self::Grpc(grpc) => format!("grpc|{}|{}", grpc.service, grpc.method),
        }
    }
}

/// A management-plane route: a match rule bound to one upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated UUID (also addressable as an anonymous GTS id).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The upstream this route forwards to.
    pub upstream_id: Uuid,
    /// Optional stable route name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Free-form lower-case labels.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Match rule.
    pub matcher: RouteMatcher,
    /// Lower runs first when several routes match.
    #[serde(default)]
    pub priority: i32,
    /// Disabled routes are not matched by the proxy.
    pub enabled: bool,
    /// Optional plugin chain overriding the upstream's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Optional rate limit overriding the upstream's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Optional CORS policy overriding the upstream's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation instant, unix epoch milliseconds.
    pub created_at: u64,
    /// Last mutation instant, unix epoch milliseconds.
    pub updated_at: u64,
}

impl Route {
    /// Validate the route's own invariants (not cross-resource ones).
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`].
    pub fn validate(&self) -> Result<(), DomainError> {
        self.matcher.validate()?;
        if let Some(name) = &self.name
            && name.trim().is_empty()
        {
            return Err(DomainError::field(
                "name",
                "name.empty",
                "route name must not be empty",
            ));
        }
        validate_tags(&self.tags)?;
        if let Some(plugins) = &self.plugins {
            for binding in &plugins.items {
                if binding.plugin_ref.trim().is_empty() {
                    return Err(DomainError::field(
                        "plugins.items",
                        reason::PLUGIN_UNKNOWN,
                        "plugin reference must not be empty",
                    ));
                }
                validate_config_object("plugins.items.config", &binding.config)?;
            }
        }
        if let Some(rate_limit) = &self.rate_limit {
            rate_limit.validate()?;
        }
        if let Some(cors) = &self.cors {
            cors.validate()?;
        }
        Ok(())
    }

    /// The route's anonymous GTS id.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts::gts_id(crate::domain::gts::ROUTE_TYPE, &self.id)
    }

    /// The per-upstream uniqueness key of this route: priority followed by
    /// the match key (`DESIGN.md` "Key Invariants": no two routes under the
    /// same upstream may share `(path prefix, priority)` for the same
    /// method).
    #[must_use]
    pub fn uniqueness_key(&self) -> String {
        format!("{}|{}", self.priority, self.matcher.match_key())
    }
}

/// Plugin family of a custom plugin row.
///
/// The wire name is the family name (`auth`, `guard`, `transform`); the GTS
/// base type is `gts.cf.core.oagw.<family>_plugin.v1~<uuid>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// Credential injection; one per upstream.
    Auth,
    /// Validation / policy enforcement; many per upstream or route.
    Guard,
    /// Request / response mutation; many per upstream or route.
    Transform,
}

impl PluginKind {
    /// GTS base type of the family (before the `~<instance>` suffix).
    #[must_use]
    pub const fn base_type(self) -> &'static str {
        match self {
            Self::Auth => "cf.core.oagw.auth_plugin.v1",
            Self::Guard => "cf.core.oagw.guard_plugin.v1",
            Self::Transform => "cf.core.oagw.transform_plugin.v1",
        }
    }

    /// Family name as it appears on the wire (`"type": "guard"`).
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The anonymous GTS id of a UUID-backed plugin of this family.
    #[must_use]
    pub fn gts_id(self, uuid: &Uuid) -> String {
        crate::domain::gts::gts_id(self.base_type(), uuid)
    }

    /// Parse a family from its wire name, its GTS base type, or a bare
    /// `cf.core.oagw.<family>_plugin.v1` string.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] on an unknown family.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let normalized = raw.trim().to_ascii_lowercase();
        for kind in [Self::Auth, Self::Guard, Self::Transform] {
            if normalized == kind.wire_name() || normalized == kind.base_type() {
                return Ok(kind);
            }
        }
        Err(DomainError::field(
            "type",
            crate::domain::reason::PROTOCOL_UNKNOWN,
            format!("'{raw}' is not an oagw plugin family (expected auth, guard or transform)"),
        ))
    }
}

impl Serialize for PluginKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire_name())
    }
}

impl<'de> Deserialize<'de> for PluginKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A custom, tenant-defined plugin row (Starlark source plus configuration).
///
/// Custom plugins are immutable after creation and are addressable as
/// `gts.cf.core.oagw.<type>_plugin.v1~<uuid>`. Named built-in plugins are
/// resolved from the in-process catalog and are never stored here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// Server-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Tenant-unique plugin name (`UNIQUE (tenant_id, name)`).
    pub name: String,
    /// Plugin family, serialized as `type`.
    #[serde(rename = "type")]
    pub kind: PluginKind,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Plugin configuration document (JSON object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Sandboxed Starlark source of the plugin.
    pub source: String,
    /// Creation instant, unix epoch milliseconds.
    pub created_at: u64,
    /// Last mutation instant, unix epoch milliseconds.
    pub updated_at: u64,
}

impl Plugin {
    /// Validate the plugin row.
    ///
    /// # Errors
    ///
    /// [`DomainError::FieldViolation`] on an empty name or source.
    pub fn validate(&self) -> Result<(), DomainError> {
        if self.name.trim().is_empty() {
            return Err(DomainError::field(
                "name",
                reason::MISSING,
                "plugin name must not be empty",
            ));
        }
        if self.source.trim().is_empty() {
            return Err(DomainError::field(
                "source",
                reason::MISSING,
                "plugin source must not be empty",
            ));
        }
        validate_config_object("config", &self.config)?;
        Ok(())
    }

    /// The plugin's anonymous GTS id (`gts.cf.core.oagw.<type>_plugin.v1~<uuid>`).
    #[must_use]
    pub fn gts_id(&self) -> String {
        self.kind.gts_id(&self.id)
    }
}

/// Validate that a JSON config field, when present, is an object.
///
/// # Errors
///
/// [`DomainError::FieldViolation`] when the value is not a JSON object.
pub fn validate_config_object(
    field: &'static str,
    value: &Option<serde_json::Value>,
) -> Result<(), DomainError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::Object(_)) => Ok(()),
        Some(_) => Err(DomainError::field(
            field,
            "config.not_object",
            format!("{field} must be a JSON object"),
        )),
    }
}

/// Validate an alias against [`ALIAS_PATTERN`] semantics.
///
/// # Errors
///
/// [`DomainError::FieldViolation`] with [`reason::ALIAS_FORMAT`].
pub fn validate_alias(alias: &str) -> Result<(), DomainError> {
    if alias.is_empty() {
        return Err(DomainError::field(
            "alias",
            reason::ALIAS_FORMAT,
            "alias must not be empty",
        ));
    }
    if alias.len() > ALIAS_MAX_LEN {
        return Err(DomainError::field(
            "alias",
            reason::ALIAS_FORMAT,
            format!("alias must be at most {ALIAS_MAX_LEN} characters"),
        ));
    }
    let bytes = alias.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(DomainError::field(
            "alias",
            reason::ALIAS_FORMAT,
            format!("alias '{alias}' must start with a lower-case letter or digit"),
        ));
    }
    if !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(DomainError::field(
            "alias",
            reason::ALIAS_FORMAT,
            format!("alias '{alias}' must end with a lower-case letter or digit"),
        ));
    }
    let ok = bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(DomainError::field(
            "alias",
            reason::ALIAS_FORMAT,
            format!("alias '{alias}' must match {ALIAS_PATTERN}"),
        ))
    }
}

/// Validate the tag list against [`TAG_PATTERN`].
///
/// # Errors
///
/// [`DomainError::FieldViolation`] with [`reason::TAG_FORMAT`].
pub fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        let ok = !tag.is_empty()
            && tag
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !ok {
            return Err(DomainError::field(
                "tags",
                reason::TAG_FORMAT,
                format!("tag '{tag}' must match {TAG_PATTERN}"),
            ));
        }
    }
    Ok(())
}

/// Wall-clock instant in unix epoch milliseconds. Timestamps carry no
/// timezone in the management API; they are opaque ordering stamps.
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
