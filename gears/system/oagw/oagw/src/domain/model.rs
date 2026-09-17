//! Typed model for the OAGW control plane: upstreams, endpoint pools, routes,
//! plugin bindings, rate limits and CORS.
//!
//! Two shapes exist per resource:
//!
//! - the `*Spec` types (`UpstreamSpec`, `RouteSpec`), which deserialize exactly
//!   what a client submits. Their serde attributes mirror the wire schemas in
//!   `docs/schemas/` field by field — `deny_unknown_fields` is applied exactly
//!   where the schema declares `additionalProperties: false`, and every
//!   documented default is applied through `#[serde(default)]`.
//! - the validated types (`UpstreamConfig`, `RouteConfig` plus the stored
//!   `Upstream` / `Route`), produced by [`crate::domain::validation`] with
//!   defaults materialized, the alias resolved and the match rule resolved to
//!   one variant. These are what [`crate::domain::store`] keeps in memory and
//!   what later phases consume.
//!
//! Field-level validation lives in [`crate::domain::validation`]; this module
//! only owns the shape of the data and the constructors that cannot fail
//! (`Host::parse` / `PluginRef::parse` are the exceptions: they are typed
//! constructors whose failure mode *is* a validation failure).

use std::collections::BTreeMap;
use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::GatewayError;

/// Allowed alias shape (upstream schema `alias.pattern`).
pub const ALIAS_PATTERN: &str = "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$";

/// Allowed tag shape (upstream and route schemas, `tags.items.pattern`).
pub const TAG_PATTERN: &str = "^[a-z0-9_-]+$";

/// GTS base type of an upstream (DESIGN.md §3.1).
pub const UPSTREAM_BASE_TYPE: &str = "gts.cf.core.oagw.upstream.v1";

/// GTS base type of a route (DESIGN.md §3.1).
pub const ROUTE_BASE_TYPE: &str = "gts.cf.core.oagw.route.v1";

/// `upstream.protocol` value for HTTP upstreams (upstream schema `protocol`).
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// `upstream.protocol` value for gRPC upstreams (upstream schema `protocol`).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Lowest legal endpoint port (upstream schema `port.minimum`).
pub const MIN_PORT: u16 = 1;

/// Highest legal endpoint port (upstream schema `port.maximum`).
pub const MAX_PORT: u16 = 65_535;

/// Port used when an endpoint omits `port` (upstream schema `port.default`).
pub const DEFAULT_ENDPOINT_PORT: u16 = 443;

/// Standard plaintext HTTP port, omitted from a derived alias.
pub const STANDARD_HTTP_PORT: u16 = 80;

/// Standard TLS port, omitted from a derived alias (HTTPS, WSS, WebTransport
/// and gRPC all use it).
pub const STANDARD_TLS_PORT: u16 = 443;

/// Maximum length of a hostname, excluding the trailing dot (RFC 1123).
pub const MAX_HOSTNAME_LENGTH: usize = 253;

/// Maximum length of a single hostname label (RFC 1123).
pub const MAX_HOSTNAME_LABEL_LENGTH: usize = 63;

/// Tokens consumed per request when `rate_limit.cost` is omitted.
pub const DEFAULT_COST: u32 = 1;

/// `true` for `#[serde(default = "default_true")]`.
fn default_true() -> bool {
    true
}

/// `1` for `#[serde(default = "default_cost")]`.
const fn default_cost() -> u32 {
    DEFAULT_COST
}

/// `443` for `#[serde(default = "default_endpoint_port")]`.
const fn default_endpoint_port() -> u16 {
    DEFAULT_ENDPOINT_PORT
}

/// `["GET", "POST"]` for `#[serde(default = "default_allowed_methods")]`.
fn default_allowed_methods() -> Vec<HttpMethod> {
    vec![HttpMethod::Get, HttpMethod::Post]
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Transport scheme of a single endpoint.
///
/// The wire schema declares `https`, `wss`, `wt` and `grpc`; `http` is
/// additionally accepted so a plaintext upstream can be declared. Whether such
/// a connection is actually dialled is *not* decided here: that gate is
/// [`OagwConfig::allow_http_upstream`](crate::config::OagwConfig::allow_http_upstream),
/// checked by the data plane before it opens a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP. Accepted by validation; dialling is gated by
    /// `allow_http_upstream`.
    Http,
    /// HTTP over TLS. Default.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over HTTP/2.
    Grpc,
}

impl Scheme {
    /// The wire spelling of this scheme.
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

    /// Whether this scheme dialles a plaintext (non-TLS) connection.
    ///
    /// Only [`Scheme::Http`] is plaintext; acceptance of the scheme is separate
    /// from permission to dial it.
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http)
    }

    /// The port that is considered "standard" (and therefore omitted from a
    /// derived alias) for this scheme.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => STANDARD_HTTP_PORT,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => STANDARD_TLS_PORT,
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Wire protocol used to talk to an upstream (upstream schema `protocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Protocol {
    /// HTTP/1.1 and HTTP/2 request/response proxying.
    #[default]
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC proxying.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// The GTS identifier carried on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sharing mode of a hierarchical configuration field (PRD.md §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants. Default.
    #[default]
    Private,
    /// Visible; descendants may override.
    Inherit,
    /// Visible; descendants cannot override.
    Enforce,
}

/// Rate limiting algorithm (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Allows bursts up to the bucket capacity. Default.
    #[default]
    TokenBucket,
    /// No boundary bursts.
    SlidingWindow,
}

/// Window unit of `rate_limit.sustained` (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// Tokens replenished per second. Default.
    #[default]
    Second,
    /// Tokens replenished per minute.
    Minute,
    /// Tokens replenished per hour.
    Hour,
    /// Tokens replenished per day.
    Day,
}

impl RateWindow {
    /// Length of this window in seconds, used to compare rates across windows.
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

/// Scope of the rate limit counters (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant. Default.
    #[default]
    Tenant,
    /// One counter per authenticated user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when the rate limit is exhausted (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Answer 429. Default.
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade the response.
    Degrade,
}

/// Which inbound headers are forwarded to the upstream (upstream schema
/// `headers.request.passthrough`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound headers. Default.
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header (hop-by-hop headers are still stripped).
    All,
}

/// HTTP method, union of the route (`methods`) and CORS (`allowed_methods`)
/// enums in the wire schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// `HEAD` (CORS only)
    Head,
    /// `OPTIONS` (CORS only)
    Options,
}

impl HttpMethod {
    /// The methods a route match rule may list (route schema
    /// `http_match.methods.items.enum`).
    pub const ROUTE_MATCH_ALLOWED: [Self; 5] =
        [Self::Get, Self::Post, Self::Put, Self::Delete, Self::Patch];

    /// The wire spelling of this method.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How `/{path_suffix}` from the proxy URL is treated (route schema
/// `path_suffix_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject any request carrying a path suffix.
    Disabled,
    /// Append the suffix to `path`. Default.
    #[default]
    Append,
}

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// A validated endpoint host: an RFC 1123 hostname or an IPv4/IPv6 literal,
/// normalized to ASCII lowercase with the trailing dot stripped.
///
/// Deserialization is deliberately permissive (any string deserializes) so a
/// bad host surfaces as a [`GatewayError`] from
/// [`crate::domain::validation`] rather than as a serde error; the store only
/// ever receives hosts that went through [`Host::parse`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Host(String);

impl Host {
    /// Validates `raw` as an RFC 1123 hostname or an IP literal and returns the
    /// normalized form.
    ///
    /// RFC 1123 rules (DESIGN.md §3.1 "Hostname Validation"): at most
    /// [`MAX_HOSTNAME_LENGTH`] characters, labels of 1-63 characters, ASCII
    /// alphanumerics and hyphens only, no leading or trailing hyphen. A single
    /// trailing dot is tolerated and stripped. IPv4 and IPv6 literals are
    /// accepted in place of a hostname; a bracketed IPv6 literal is stored
    /// bare.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`GatewayError`] when `raw` is empty, is a malformed IP
    /// literal, or violates any RFC 1123 hostname rule.
    pub fn parse(raw: &str) -> Result<Self, GatewayError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(GatewayError::validation("host must not be empty", "host"));
        }

        // FQDN notation: tolerate one trailing dot.
        let stripped = trimmed.strip_suffix('.').unwrap_or(trimmed);
        let normalized = stripped.to_ascii_lowercase();

        if normalized.len() > MAX_HOSTNAME_LENGTH {
            return Err(GatewayError::validation(
                format!("host exceeds {MAX_HOSTNAME_LENGTH} characters"),
                "host",
            ));
        }

        if normalized.starts_with('[') {
            return Self::parse_bracketed_ipv6(&normalized);
        }

        if let Ok(address) = normalized.parse::<Ipv4Addr>() {
            return Ok(Self(address.to_string()));
        }

        if normalized.parse::<Ipv6Addr>().is_ok() {
            return Ok(Self(normalized));
        }

        if is_dotted_quad(&normalized) {
            return Err(GatewayError::validation(
                format!("`{normalized}` is not a valid IPv4 address"),
                "host",
            ));
        }

        validate_hostname_labels(&normalized)?;

        Ok(Self(normalized))
    }

    fn parse_bracketed_ipv6(normalized: &str) -> Result<Self, GatewayError> {
        let Some(inner) = normalized
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        else {
            return Err(GatewayError::validation(
                format!("`{normalized}` is not a valid IPv6 literal"),
                "host",
            ));
        };

        if inner.parse::<Ipv6Addr>().is_err() {
            return Err(GatewayError::validation(
                format!("`{inner}` is not a valid IPv6 literal"),
                "host",
            ));
        }

        Ok(Self(inner.to_owned()))
    }

    /// The normalized host as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this host is an IPv4 or IPv6 literal (and therefore needs an
    /// explicit alias).
    #[must_use]
    pub fn is_ip_literal(&self) -> bool {
        self.is_ipv4() || self.is_ipv6()
    }

    /// Whether this host is an IPv4 literal.
    #[must_use]
    pub fn is_ipv4(&self) -> bool {
        self.0.parse::<Ipv4Addr>().is_ok()
    }

    /// Whether this host is an IPv6 literal.
    #[must_use]
    pub fn is_ipv6(&self) -> bool {
        self.0.parse::<Ipv6Addr>().is_ok()
    }

    /// Consumes the host and returns the inner normalized string.
    #[must_use]
    pub fn into_inner(self) -> String {
        self.0
    }

    /// The `host:port` authority used to build the outbound URL, with IPv6
    /// literals bracketed.
    #[must_use]
    pub fn with_port(&self, port: u16) -> String {
        if self.is_ipv6() {
            format!("[{}]:{port}", self.0)
        } else {
            format!("{}:{port}", self.0)
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `true` when `host` has exactly four all-numeric labels, i.e. it was meant to
/// be an IPv4 address and must not silently pass as a hostname.
fn is_dotted_quad(host: &str) -> bool {
    let labels = host.split('.').count();
    labels == 4
        && host
            .split('.')
            .all(|label| !label.is_empty() && label.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Applies the RFC 1123 label rules to an already-normalized hostname.
fn validate_hostname_labels(host: &str) -> Result<(), GatewayError> {
    for label in host.split('.') {
        if label.is_empty() {
            return Err(GatewayError::validation(
                "host contains an empty label",
                "host",
            ));
        }
        if label.len() > MAX_HOSTNAME_LABEL_LENGTH {
            return Err(GatewayError::validation(
                format!("host label `{label}` exceeds {MAX_HOSTNAME_LABEL_LENGTH} characters"),
                "host",
            ));
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(GatewayError::validation(
                format!("host label `{label}` may only contain alphanumerics and hyphens"),
                "host",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(GatewayError::validation(
                format!("host label `{label}` must not start or end with a hyphen"),
                "host",
            ));
        }
    }

    Ok(())
}

/// A plugin reference: either a builtin GTS identifier or a custom plugin UUID
/// (upstream schema `plugins.items[].oneOf`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginRef(String);

impl PluginRef {
    /// Validates `raw` as a builtin GTS identifier or a custom plugin UUID.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`GatewayError`] when `raw` is neither a GTS identifier
    /// (`gts.<chain>~<instance>`) nor a UUID.
    pub fn parse(raw: &str) -> Result<Self, GatewayError> {
        let trimmed = raw.trim();
        if Uuid::parse_str(trimmed).is_ok() {
            return Ok(Self(trimmed.to_owned()));
        }
        if is_gts_identifier(trimmed) {
            return Ok(Self(trimmed.to_owned()));
        }

        Err(GatewayError::validation(
            format!(
                "`{raw}` is not a valid plugin reference (expected a GTS identifier or a UUID)"
            ),
            "plugins",
        ))
    }

    /// The reference as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this reference resolves through the in-process plugin registry
    /// (a named plugin) rather than through stored custom plugins.
    #[must_use]
    pub fn is_named(&self) -> bool {
        is_gts_identifier(&self.0)
    }

    /// Whether this reference addresses a stored custom plugin by UUID.
    #[must_use]
    pub fn is_custom(&self) -> bool {
        Uuid::parse_str(&self.0).is_ok()
    }

    /// The UUID of a custom plugin reference.
    #[must_use]
    pub fn custom_uuid(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.0).ok()
    }
}

impl fmt::Display for PluginRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Structural GTS identifier check: `gts.` prefix, a `~` separating the type
/// chain from the instance, and no empty halves. Deep resolution against the
/// plugin registries is not part of validation.
fn is_gts_identifier(raw: &str) -> bool {
    let Some((base, instance)) = raw.split_once('~') else {
        return false;
    };

    raw.starts_with("gts.")
        && !base.is_empty()
        && !instance.is_empty()
        && base.len() > "gts.".len()
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'~' | b'-' | b'_'))
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// One member of an endpoint pool (upstream schema `server.endpoints[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Transport scheme. Defaults to [`Scheme::Https`].
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname or IP literal. Required.
    pub host: Host,
    /// Port. Defaults to [`DEFAULT_ENDPOINT_PORT`].
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
}

impl Endpoint {
    /// Builds an endpoint from already-validated parts.
    #[must_use]
    pub const fn new(scheme: Scheme, host: Host, port: u16) -> Self {
        Self { scheme, host, port }
    }

    /// Whether the endpoint uses a plaintext scheme ([`Scheme::Http`]).
    #[must_use]
    pub const fn is_plaintext(&self) -> bool {
        self.scheme.is_plaintext()
    }
}

/// The endpoint pool of an upstream (upstream schema `server`). At least one
/// endpoint is required and every endpoint must share protocol, scheme and
/// port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoint pool. Required, at least one entry.
    pub endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// Builds a pool from the given endpoints.
    #[must_use]
    pub const fn new(endpoints: Vec<Endpoint>) -> Self {
        Self { endpoints }
    }
}

/// Upstream authentication plugin binding (upstream schema `auth`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Auth plugin type (GTS identifier or custom plugin UUID).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<PluginRef>,
    /// Sharing mode for hierarchical configuration. Defaults to
    /// [`SharingMode::Private`].
    #[serde(default)]
    pub sharing: SharingMode,
    /// Authentication plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Header transformation rules (upstream schema `definitions.headers`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HeadersConfig {
    /// Rules applied to the request forwarded upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaders>,
    /// Rules applied to the response returned to the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaders>,
}

/// Inbound request header rules (upstream schema `definitions.headers.request`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RequestHeaders {
    /// Headers to set (overwrite if present).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward. Defaults to [`PassthroughMode::None`].
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough` is [`PassthroughMode::Allowlist`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header rules (upstream schema `definitions.headers.response`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ResponseHeaders {
    /// Headers to set on the response to the client.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Headers to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Plugin chain binding (upstream and route schemas, `plugins`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Sharing mode for the plugin chain. Defaults to [`SharingMode::Private`].
    #[serde(default)]
    pub sharing: SharingMode,
    /// Builtin plugins by GTS identifier, custom plugins by UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginRef>,
}

/// Sustained rate of a rate limit (ADR 0003). `rate` is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per [`Self::window`]. Required, at least 1.
    pub rate: u32,
    /// Window unit. Defaults to [`RateWindow::Second`].
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst bucket of a rate limit (ADR 0003). An omitted capacity falls back to
/// [`RateLimitConfig::burst_capacity`], i.e. the sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BurstCapacity {
    /// Maximum burst size. Defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Rate limiting configuration (ADR 0003, upstream and route schema
/// `definitions.rate_limit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode. Defaults to [`SharingMode::Private`].
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm. Defaults to [`RateLimitAlgorithm::TokenBucket`].
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate. Required.
    pub sustained: SustainedRate,
    /// Burst bucket. An omitted capacity defaults to the sustained rate.
    #[serde(default)]
    pub burst: BurstCapacity,
    /// Counter scope. Defaults to [`RateScope::Tenant`].
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour when exhausted. Defaults to [`RateStrategy::Reject`].
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request. Defaults to [`DEFAULT_COST`].
    #[serde(default = "default_cost")]
    pub cost: u32,
}

impl RateLimitConfig {
    /// The effective burst capacity: `burst.capacity` when set, otherwise the
    /// sustained rate (ADR 0003 field table).
    #[must_use]
    pub fn burst_capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate)
    }

    /// The sustained rate expressed in tokens per second, as an exact
    /// rational numerator (`rate / window_seconds`). Used to compare limits
    /// declared with different windows without floating point.
    #[must_use]
    pub fn per_second(&self) -> RatePerSecond {
        RatePerSecond {
            rate: u64::from(self.sustained.rate),
            window_seconds: self.sustained.window.seconds(),
        }
    }
}

/// Exact `rate / window_seconds` numerator pair, kept as integers so two
/// limits can be compared without precision loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RatePerSecond {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window length in seconds.
    pub window_seconds: u64,
}

impl RatePerSecond {
    /// Compares two rates without floating point: `a` is stricter than `b`
    /// when `a.rate / a.window < b.rate / b.window`.
    #[must_use]
    pub fn is_stricter_than(self, other: Self) -> bool {
        self.rate * other.window_seconds < other.rate * self.window_seconds
    }
}

/// CORS configuration (ADR 0004, upstream and route schema
/// `definitions.cors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode. Defaults to [`SharingMode::Private`].
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS is enabled. Defaults to `false` (secure by default).
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins: `*` or an absolute URI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed methods. Defaults to `GET` and `POST`.
    #[serde(
        default = "default_allowed_methods",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_methods: Vec<HttpMethod>,
    /// Headers exposed to the browser beyond the CORS-safelisted ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed. Must not be combined with a wildcard
    /// origin.
    #[serde(default)]
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    /// The serde defaults: disabled, no credentials, `GET` and `POST` allowed.
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_allowed_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl CorsConfig {
    /// Whether the wildcard origin is allowed.
    #[must_use]
    pub fn allows_any_origin(&self) -> bool {
        self.allowed_origins.iter().any(|origin| origin == "*")
    }
}

/// Inbound match rules of a route (route schema `match`). Exactly one of the
/// `http` / `grpc` members must be present (route schema `oneOf`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchSpec {
    /// HTTP match rules.
    #[serde(default, rename = "http", skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, rename = "grpc", skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchSpec {
    /// The single match rule this spec declares, if exactly one is present.
    #[must_use]
    pub fn as_rule(&self) -> Option<MatchRule> {
        match (self.http.as_ref(), self.grpc.as_ref()) {
            (Some(http), None) => Some(MatchRule::Http(http.clone())),
            (None, Some(grpc)) => Some(MatchRule::Grpc(grpc.clone())),
            _ => None,
        }
    }
}

/// Resolved match rule of a stored route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchRule {
    /// HTTP method and path matching.
    Http(HttpMatch),
    /// gRPC service and method matching.
    Grpc(GrpcMatch),
}

impl MatchRule {
    /// The HTTP match rule, when this is an HTTP route.
    #[must_use]
    pub const fn as_http(&self) -> Option<&HttpMatch> {
        match self {
            Self::Http(matched) => Some(matched),
            Self::Grpc(_) => None,
        }
    }

    /// The gRPC match rule, when this is a gRPC route.
    #[must_use]
    pub const fn as_grpc(&self) -> Option<&GrpcMatch> {
        match self {
            Self::Grpc(matched) => Some(matched),
            Self::Http(_) => None,
        }
    }
}

/// HTTP match rules (route schema `definitions.http_match`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods this route accepts. Required, at least one.
    pub methods: Vec<HttpMethod>,
    /// Path pattern. Required, non-empty.
    pub path: String,
    /// Allowed query parameters; empty means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` is treated. Defaults to
    /// [`PathSuffixMode::Append`].
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (route schema `definitions.grpc_match`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name. Required, non-empty.
    pub service: String,
    /// RPC method name. Required, non-empty.
    pub method: String,
}

// ---------------------------------------------------------------------------
// Upstream document shapes
// ---------------------------------------------------------------------------

/// An upstream exactly as submitted (upstream schema root).
///
/// `id` and `tenant_id` are absent on purpose: `id` is system-generated and
/// `tenant_id` comes from the authenticated caller, so the schema's
/// `additionalProperties: false` forbids both on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    /// Routing identifier. Optional: derived from the endpoints when they
    /// allow it, required for IP-based or non-derivable pools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Flat tags for categorization and discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool. Required.
    pub server: ServerConfig,
    /// Upstream protocol. Required.
    pub protocol: Protocol,
    /// Authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain. Defaults to an empty chain with
    /// [`SharingMode::Private`].
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// A validated upstream: defaults materialized, the alias resolved and every
/// pool and policy rule checked. Assigned an `id` and a `tenant_id` by the
/// store.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpstreamConfig {
    /// Resolved, normalized routing alias.
    pub alias: String,
    /// Whether the upstream is enabled.
    pub enabled: bool,
    /// Tenant-local tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Authentication plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate limiting configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
}

/// A stored upstream (DESIGN.md §3.1 `Upstream`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Validated configuration.
    #[serde(flatten)]
    pub config: UpstreamConfig,
}

impl Upstream {
    /// Builds a stored upstream from its identity and validated configuration.
    #[must_use]
    pub const fn new(id: Uuid, tenant_id: Uuid, config: UpstreamConfig) -> Self {
        Self {
            id,
            tenant_id,
            config,
        }
    }

    /// The resolved routing alias.
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.config.alias
    }

    /// The endpoint pool.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.config.server.endpoints
    }

    /// Whether the upstream accepts traffic.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

// ---------------------------------------------------------------------------
// Route document shapes
// ---------------------------------------------------------------------------

/// A route exactly as submitted (route schema `properties`).
///
/// The route schema declares no `additionalProperties: false` at the top
/// level, so unknown members are tolerated here; every nested object
/// (`match`, `http_match`, `grpc_match`, `rate_limit`, `cors`) rejects them.
/// `enabled` and `priority` come from the DESIGN.md §3.1 `Route` entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteSpec {
    /// Upstream this route belongs to. Required, immutable on update.
    pub upstream_id: Uuid,
    /// Match rules. Required, exactly one of `http` / `grpc`.
    #[serde(rename = "match")]
    pub match_spec: MatchSpec,
    /// Whether the route accepts traffic. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match priority. Defaults to `0`.
    #[serde(default)]
    pub priority: u32,
    /// Tags for categorization and discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Plugin chain. Defaults to an empty chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Route-level rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for RouteSpec {
    fn default() -> Self {
        Self {
            upstream_id: Uuid::nil(),
            match_spec: MatchSpec::default(),
            enabled: true,
            priority: 0,
            tags: Vec::new(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

/// A validated route: the match rule resolved to exactly one variant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RouteConfig {
    /// Upstream this route belongs to.
    pub upstream_id: Uuid,
    /// Resolved match rule.
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    /// Whether the route accepts traffic.
    pub enabled: bool,
    /// Match priority.
    pub priority: u32,
    /// Tenant-local tags.
    pub tags: Vec<String>,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    pub cors: Option<CorsConfig>,
}

/// A stored route (DESIGN.md §3.1 `Route`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Validated configuration.
    #[serde(flatten)]
    pub config: RouteConfig,
}

impl Route {
    /// Builds a stored route from its identity and validated configuration.
    #[must_use]
    pub const fn new(id: Uuid, tenant_id: Uuid, config: RouteConfig) -> Self {
        Self {
            id,
            tenant_id,
            config,
        }
    }

    /// The owning upstream.
    #[must_use]
    pub const fn upstream_id(&self) -> Uuid {
        self.config.upstream_id
    }

    /// Whether the route accepts traffic.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse<T: serde::de::DeserializeOwned>(
        body: serde_json::Value,
    ) -> Result<T, serde_json::Error> {
        serde_json::from_value(body)
    }

    // -- endpoint shape ---------------------------------------------------

    #[test]
    fn test_endpoint_defaults_to_https_and_443() {
        let endpoint: Endpoint = parse(json!({ "host": "api.openai.com" })).unwrap();

        assert_eq!(endpoint.scheme, Scheme::Https);
        assert_eq!(endpoint.port, DEFAULT_ENDPOINT_PORT);
        assert_eq!(endpoint.host.as_str(), "api.openai.com");
        assert!(!endpoint.is_plaintext());
    }

    #[test]
    fn test_endpoint_rejects_unknown_members() {
        let error = parse::<Endpoint>(json!({
            "host": "api.openai.com",
            "weight": 3
        }))
        .unwrap_err();

        assert!(error.to_string().contains("weight"), "{error}");
    }

    #[test]
    fn test_server_requires_at_least_the_endpoints_member() {
        let error =
            parse::<ServerConfig>(json!({ "endpoints": [], "lb": "round_robin" })).unwrap_err();

        assert!(error.to_string().contains("lb"), "{error}");
    }

    // -- scheme -----------------------------------------------------------

    #[test]
    fn test_scheme_accepts_every_documented_value_including_http() {
        for (wire, expected) in [
            ("http", Scheme::Http),
            ("https", Scheme::Https),
            ("wss", Scheme::Wss),
            ("wt", Scheme::Wt),
            ("grpc", Scheme::Grpc),
        ] {
            let endpoint: Endpoint =
                parse(json!({ "scheme": wire, "host": "api.openai.com" })).unwrap();

            assert_eq!(endpoint.scheme, expected, "{wire}");
            assert_eq!(endpoint.scheme.as_str(), wire);
            assert_eq!(endpoint.scheme.to_string(), wire);
        }

        assert!(parse::<Endpoint>(json!({ "scheme": "ftp", "host": "api.openai.com" })).is_err());
    }

    #[test]
    fn test_scheme_defaults_to_https_and_marks_http_plaintext() {
        assert_eq!(Scheme::default(), Scheme::Https);
        assert!(Scheme::Http.is_plaintext());
        assert!(!Scheme::Https.is_plaintext());
        assert!(!Scheme::Grpc.is_plaintext());
    }

    #[test]
    fn test_standard_ports_per_scheme() {
        assert_eq!(Scheme::Http.standard_port(), STANDARD_HTTP_PORT);
        assert_eq!(Scheme::Http.standard_port(), 80);
        for scheme in [Scheme::Https, Scheme::Wss, Scheme::Wt, Scheme::Grpc] {
            assert_eq!(scheme.standard_port(), STANDARD_TLS_PORT);
            assert_eq!(scheme.standard_port(), 443);
        }
    }

    // -- protocol ---------------------------------------------------------

    #[test]
    fn test_protocol_round_trips_the_gts_identifiers() {
        assert_eq!(
            PROTOCOL_HTTP,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
        assert_eq!(
            PROTOCOL_GRPC,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
        );

        for (wire, expected) in [
            (PROTOCOL_HTTP, Protocol::Http),
            (PROTOCOL_GRPC, Protocol::Grpc),
        ] {
            assert_eq!(parse::<Protocol>(json!(wire)).unwrap(), expected);
            assert_eq!(expected.as_str(), wire);
        }

        assert!(parse::<Protocol>(json!("http")).is_err());
    }

    // -- sharing modes and rate limit enums -------------------------------

    #[test]
    fn test_sharing_mode_round_trips() {
        assert_eq!(SharingMode::default(), SharingMode::Private);
        for (wire, expected) in [
            ("private", SharingMode::Private),
            ("inherit", SharingMode::Inherit),
            ("enforce", SharingMode::Enforce),
        ] {
            assert_eq!(parse::<SharingMode>(json!(wire)).unwrap(), expected);
        }

        assert!(parse::<SharingMode>(json!("public")).is_err());
    }

    #[test]
    fn test_rate_window_seconds() {
        for (wire, seconds) in [
            ("second", 1),
            ("minute", 60),
            ("hour", 3_600),
            ("day", 86_400),
        ] {
            let window = parse::<RateWindow>(json!(wire)).unwrap();
            assert_eq!(window.seconds(), seconds);
        }

        assert_eq!(RateWindow::default(), RateWindow::Second);
    }

    #[test]
    fn test_rate_limit_defaults() {
        let limit: RateLimitConfig = parse(json!({ "sustained": { "rate": 100 } })).unwrap();

        assert_eq!(limit.sharing, SharingMode::Private);
        assert_eq!(limit.algorithm, RateLimitAlgorithm::TokenBucket);
        assert_eq!(limit.sustained.window, RateWindow::Second);
        assert_eq!(limit.scope, RateScope::Tenant);
        assert_eq!(limit.strategy, RateStrategy::Reject);
        assert_eq!(limit.cost, DEFAULT_COST);
        assert_eq!(limit.burst_capacity(), 100);
    }

    #[test]
    fn test_rate_limit_burst_capacity_falls_back_to_the_sustained_rate() {
        let limit: RateLimitConfig = parse(json!({
            "sustained": { "rate": 40, "window": "minute" },
            "burst": { "capacity": 200 }
        }))
        .unwrap();

        assert_eq!(limit.burst_capacity(), 200);
        assert_eq!(limit.per_second().rate, 40);
        assert_eq!(limit.per_second().window_seconds, 60);
    }

    #[test]
    fn test_rate_per_second_compares_exactly() {
        // 1/minute < 1/second, even though both numerators are 1.
        let per_minute = RatePerSecond {
            rate: 1,
            window_seconds: 60,
        };
        let per_second = RatePerSecond {
            rate: 1,
            window_seconds: 1,
        };

        assert!(per_minute.is_stricter_than(per_second));
        assert!(!per_second.is_stricter_than(per_minute));
        assert!(!per_minute.is_stricter_than(per_minute));
    }

    #[test]
    fn test_rate_limit_rejects_unknown_members() {
        let error = parse::<RateLimitConfig>(json!({
            "sustained": { "rate": 1 },
            "retry_after": 30
        }))
        .unwrap_err();

        assert!(error.to_string().contains("retry_after"), "{error}");
    }

    // -- CORS -------------------------------------------------------------

    #[test]
    fn test_cors_defaults_to_disabled() {
        let cors = CorsConfig::default();

        assert!(!cors.enabled);
        assert!(!cors.allow_credentials);
        assert_eq!(
            cors.allowed_methods,
            vec![HttpMethod::Get, HttpMethod::Post]
        );
        assert!(!cors.allows_any_origin());
    }

    #[test]
    fn test_cors_detects_the_wildcard_origin() {
        let cors: CorsConfig = parse(json!({
            "enabled": true,
            "allowed_origins": ["https://console.example.com", "*"]
        }))
        .unwrap();

        assert!(cors.allows_any_origin());
    }

    #[test]
    fn test_cors_rejects_unknown_members() {
        let error =
            parse::<CorsConfig>(json!({ "enabled": true, "allow_headers": ["*"] })).unwrap_err();

        assert!(error.to_string().contains("allow_headers"), "{error}");
    }

    // -- headers and plugins ----------------------------------------------

    #[test]
    fn test_headers_reject_unknown_members() {
        let error = parse::<RequestHeaders>(json!({ "set": {}, "append": {} })).unwrap_err();

        assert!(error.to_string().contains("append"), "{error}");
    }

    #[test]
    fn test_plugins_default_to_an_empty_private_chain() {
        let plugins = PluginsConfig::default();

        assert_eq!(plugins.sharing, SharingMode::Private);
        assert!(plugins.items.is_empty());
    }

    const CUSTOM_PLUGIN: &str = "9b2f4a44-2c6b-4c1f-9d3d-1f0a0b6d8e11";

    #[test]
    fn test_plugin_ref_accepts_gts_identifier_and_uuid() {
        let named =
            PluginRef::parse("gts.cf.plugins.plugin.v1~cf.plugins.pii_redactor.v1").unwrap();
        assert!(named.is_named());
        assert!(!named.is_custom());
        assert_eq!(named.custom_uuid(), None);

        let custom = PluginRef::parse(CUSTOM_PLUGIN).unwrap();
        assert!(custom.is_custom());
        assert!(!named.is_custom());
        assert_eq!(
            custom.custom_uuid().map(|id| id.to_string()),
            Some(CUSTOM_PLUGIN.to_owned())
        );
    }

    #[test]
    fn test_plugin_ref_rejects_junk() {
        for raw in ["", "not-a-plugin", "gts.broken", "PII Redactor"] {
            assert!(PluginRef::parse(raw).is_err(), "{raw}");
        }
    }

    // -- host -------------------------------------------------------------

    #[test]
    fn test_host_accepts_rfc_1123_hostnames() {
        for raw in ["api.openai.com", "a", "a-b.c", "xn--nxasmq6b.example"] {
            let host = Host::parse(raw).unwrap();
            assert_eq!(host.as_str(), raw, "{raw}");
            assert!(!host.is_ip_literal());
        }
    }

    #[test]
    fn test_host_normalizes_case_and_strips_one_trailing_dot() {
        assert_eq!(
            Host::parse("Api.OpenAI.Com").unwrap().as_str(),
            "api.openai.com"
        );
        assert_eq!(
            Host::parse("api.openai.com.").unwrap().as_str(),
            "api.openai.com"
        );
        assert_eq!(
            Host::parse("  API.OpenAI.COM  ").unwrap().as_str(),
            "api.openai.com"
        );
    }

    #[test]
    fn test_host_accepts_the_longest_legal_label() {
        let label = "a".repeat(MAX_HOSTNAME_LABEL_LENGTH);
        let host = Host::parse(&format!("{label}.{label}")).unwrap();

        assert!(host.as_str().contains(&label));
        assert!(
            Host::parse(&format!(
                "api.{}",
                "a".repeat(MAX_HOSTNAME_LABEL_LENGTH + 1)
            ))
            .is_err()
        );
    }

    #[test]
    fn test_host_accepts_the_longest_legal_name() {
        let labels = [
            "a".repeat(50),
            "a".repeat(50),
            "a".repeat(50),
            "a".repeat(50),
            "a".repeat(49),
        ];
        let long = labels.join(".");

        assert_eq!(long.len(), MAX_HOSTNAME_LENGTH);
        assert_eq!(
            Host::parse(&long).unwrap().as_str().len(),
            MAX_HOSTNAME_LENGTH
        );

        let too_long = format!("{long}.a");
        assert!(Host::parse(&too_long).is_err());
    }

    #[test]
    fn test_host_rejects_malformed_hostnames() {
        for raw in [
            "",
            "   ",
            ".api",
            "-api",
            "api-",
            "api..com",
            "under_score.example.com",
            "api openai.com",
        ] {
            assert!(Host::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn test_host_rejects_overlong_names_and_labels() {
        let label = "a".repeat(MAX_HOSTNAME_LABEL_LENGTH + 1);
        assert!(Host::parse(&format!("api.{label}")).is_err());

        let long = format!("{}.", "b".repeat(MAX_HOSTNAME_LENGTH + 1));
        assert!(Host::parse(&long).is_err());
    }

    #[test]
    fn test_host_rejects_an_invalid_dotted_quad_but_accepts_a_valid_one() {
        assert!(Host::parse("999.1.1.1").is_err());
        assert!(Host::parse("1.2.3.4.5").is_ok());
        assert!(Host::parse("10.0.1.1").unwrap().is_ipv4());
    }

    #[test]
    fn test_host_accepts_ipv6_literals() {
        let host = Host::parse("[2001:db8::1]").unwrap();
        assert_eq!(host.as_str(), "2001:db8::1");
        assert!(host.is_ipv6());
        assert!(!host.is_ipv4());
        assert_eq!(host.with_port(8443), "[2001:db8::1]:8443");

        assert!(Host::parse("2001:db8::1").unwrap().is_ipv6());
        assert!(Host::parse("[2001:db8::1").is_err());
        assert!(Host::parse("[not-an-address]").is_err());
        assert_eq!(
            Host::parse("api.openai.com").unwrap().with_port(80),
            "api.openai.com:80"
        );
    }

    // -- match rules ------------------------------------------------------

    #[test]
    fn test_http_and_grpc_methods_round_trip() {
        let matched: HttpMatch = parse(json!({
            "methods": ["GET", "POST", "PUT", "PATCH", "DELETE"],
            "path": "/v1/chat"
        }))
        .unwrap();

        assert_eq!(matched.methods.len(), HttpMethod::ROUTE_MATCH_ALLOWED.len());
        assert_eq!(matched.path_suffix_mode, PathSuffixMode::Append);

        for method in HttpMethod::ROUTE_MATCH_ALLOWED {
            assert_eq!(parse::<HttpMethod>(json!(method.as_str())).unwrap(), method);
        }

        // CORS-only methods exist but a route match rule may not list them.
        assert_eq!(
            parse::<HttpMethod>(json!("HEAD")).unwrap(),
            HttpMethod::Head
        );
    }

    #[test]
    fn test_match_spec_resolves_to_exactly_one_rule() {
        let http: MatchSpec = parse(json!({
            "http": { "methods": ["GET"], "path": "/v1/chat" }
        }))
        .unwrap();
        assert!(http.as_rule().is_some());
        assert!(http.as_rule().unwrap().as_http().is_some());

        let grpc: MatchSpec = parse(json!({
            "grpc": { "service": "cf.shell.v1.Shell", "method": "Exec" }
        }))
        .unwrap();
        assert!(grpc.as_rule().unwrap().as_grpc().is_some());

        assert_eq!(MatchSpec::default().as_rule(), None);
    }

    #[test]
    fn test_match_and_grpc_match_reject_unknown_members() {
        let error = parse::<HttpMatch>(json!({ "methods": ["GET"], "path": "/v1", "query": [] }))
            .unwrap_err();
        assert!(error.to_string().contains("query"), "{error}");

        let error = parse::<GrpcMatch>(json!({ "service": "s", "method": "m", "stream": true }))
            .unwrap_err();
        assert!(error.to_string().contains("stream"), "{error}");
    }

    // -- upstream and route documents -------------------------------------

    #[test]
    fn test_upstream_requires_server_and_protocol() {
        let error = parse::<UpstreamSpec>(json!({ "server": { "endpoints": [] } })).unwrap_err();
        assert!(error.to_string().contains("protocol"), "{error}");

        let error = parse::<UpstreamSpec>(json!({ "protocol": PROTOCOL_HTTP })).unwrap_err();
        assert!(error.to_string().contains("server"), "{error}");
    }

    #[test]
    fn test_upstream_rejects_unknown_members() {
        let error = parse::<UpstreamSpec>(json!({
            "server": { "endpoints": [{ "host": "api.openai.com" }] },
            "protocol": PROTOCOL_HTTP,
            "tenant_id": "abc"
        }))
        .unwrap_err();

        assert!(error.to_string().contains("tenant_id"), "{error}");
    }

    #[test]
    fn test_upstream_spec_defaults() {
        let spec: UpstreamSpec = parse(json!({
            "server": { "endpoints": [{ "host": "api.openai.com" }] },
            "protocol": PROTOCOL_HTTP
        }))
        .unwrap();

        assert_eq!(spec.alias, None);
        assert!(spec.enabled);
        assert!(spec.tags.is_empty());
        assert_eq!(spec.plugins, PluginsConfig::default());
        assert!(spec.auth.is_none());
        assert!(spec.headers.is_none());
        assert!(spec.rate_limit.is_none());
        assert!(spec.cors.is_none());
    }

    #[test]
    fn test_upstream_and_route_carry_their_identity() {
        let id = Uuid::from_u128(7);
        let tenant = Uuid::from_u128(8);
        let config = UpstreamConfig {
            alias: "api.openai.com".to_owned(),
            enabled: false,
            tags: vec!["llm".to_owned()],
            server: ServerConfig::new(vec![Endpoint::new(
                Scheme::Https,
                Host::parse("api.openai.com").unwrap(),
                443,
            )]),
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        };
        let upstream = Upstream::new(id, tenant, config);

        assert_eq!(upstream.id, id);
        assert_eq!(upstream.tenant_id, tenant);
        assert_eq!(upstream.alias(), "api.openai.com");
        assert_eq!(upstream.endpoints().len(), 1);
        assert!(!upstream.is_enabled());
        assert_eq!(upstream.config.tags, vec!["llm".to_owned()]);

        let route = Route::new(
            id,
            tenant,
            RouteConfig {
                upstream_id: Uuid::from_u128(9),
                match_rule: MatchRule::Http(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: "/v1/chat".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                enabled: true,
                priority: 3,
                tags: Vec::new(),
                plugins: PluginsConfig::default(),
                rate_limit: None,
                cors: None,
            },
        );

        assert_eq!(route.upstream_id(), Uuid::from_u128(9));
        assert!(route.is_enabled());
        assert_eq!(route.config.priority, 3);
    }

    #[test]
    fn test_route_spec_defaults_and_tolerates_unknown_members() {
        let spec: RouteSpec = parse(json!({
            "upstream_id": Uuid::from_u128(9),
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
            "weight": 1
        }))
        .unwrap();

        assert!(spec.enabled);
        assert_eq!(spec.priority, 0);
        assert!(spec.tags.is_empty());
        assert!(spec.rate_limit.is_none());
        assert!(spec.cors.is_none());
    }
}
