// Created: 2026-09-04 by Constructor Tech
//! Upstream aggregate and its configuration value objects.
//!
//! Mirrors `docs/schemas/upstream.v1.schema.json` and the `Upstream` /
//! `ServerConfig` / `Endpoint` classes of `docs/DESIGN.md` §3.1. Only
//! invariants live here — persistence, HTTP transport and wire mapping are
//! owned by later layers.

use std::collections::BTreeMap;
use std::fmt;
use std::net::IpAddr;
use std::num::NonZeroU32;

use toolkit_gts::gts_id;

use crate::domain::alias::{Alias, resolve_alias};
use crate::domain::plugin::PluginRef;
use crate::domain::resolve_slot;
use crate::error::OagwError;

/// GTS type of the upstream resource.
pub const UPSTREAM_GTS_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");

/// GTS identifier of the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");

/// GTS identifier of the gRPC upstream protocol.
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// Maximum hostname length (RFC 1035).
pub const MAX_HOST_LEN: usize = 253;

/// Maximum length of a single hostname label.
const MAX_HOST_LABEL_LEN: usize = 63;

/// Scheme of an upstream endpoint.
///
/// `http` is a legal value: `allow_http_upstream` (`crate::config::OagwConfig`)
/// governs whether a plaintext connection is actually made at egress time, in
/// the data plane. Scheme validation never consults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum EndpointScheme {
    /// Plaintext HTTP (legal scheme value; egress gated by
    /// `allow_http_upstream`).
    Http,
    /// TLS-protected HTTP.
    #[default]
    Https,
    /// TLS-protected WebSocket.
    Wss,
    /// WebTransport (HTTP/3 over QUIC).
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl EndpointScheme {
    /// Every scheme the upstream schema admits.
    pub const ALL: [EndpointScheme; 5] = [Self::Http, Self::Https, Self::Wss, Self::Wt, Self::Grpc];

    /// Scheme token as used on the wire and in the schema enum.
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

    /// `true` for the only plaintext scheme (`http`).
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http)
    }

    /// `true` when the scheme always rides on a TLS (or QUIC) transport.
    #[must_use]
    pub const fn requires_tls(self) -> bool {
        !self.is_plaintext()
    }

    /// Port assumed when the endpoint does not declare one
    /// (HTTP: 80, everything else: 443).
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// `true` when `port` is the standard port of this scheme
    /// (`docs/schemas/upstream.v1.schema.json` alias derivation rules).
    #[must_use]
    pub const fn is_standard_port(self, port: u16) -> bool {
        self.default_port() == port
    }

    /// Parses a scheme token, case-insensitively.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::InvalidEndpoint`] for any token outside
    /// [`EndpointScheme::ALL`].
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            "wss" => Ok(Self::Wss),
            "wt" => Ok(Self::Wt),
            "grpc" => Ok(Self::Grpc),
            _ => Err(OagwError::InvalidEndpoint {
                reason: format!(
                    "unknown endpoint scheme '{raw}' (expected one of http, https, wss, wt, grpc)"
                ),
            }),
        }
    }
}

impl fmt::Display for EndpointScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for EndpointScheme {
    type Err = OagwError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse(raw)
    }
}

/// One load-balanced endpoint of an upstream pool
/// (`docs/DESIGN.md` §3.1 `Endpoint`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Endpoint {
    scheme: EndpointScheme,
    host: String,
    port: Option<u16>,
}

impl Endpoint {
    /// Builds an endpoint from its parts.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::InvalidEndpoint`] when the host is empty, not a
    /// hostname or IP address, or the port is out of range.
    pub fn new(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Result<Self, OagwError> {
        let host = normalize_host(host)?;
        if port.is_some_and(|port| port == 0) {
            return Err(OagwError::InvalidEndpoint {
                reason: String::from("the endpoint port must be between 1 and 65_535"),
            });
        }
        Ok(Self { scheme, host, port })
    }

    /// Builds an endpoint from a URL-shaped string
    /// (`https://api.example.com:8443`).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::InvalidEndpoint`] when the value is not an
    /// absolute URL with a known scheme and a host.
    pub fn parse_url(raw: &str) -> Result<Self, OagwError> {
        let parsed = url::Url::parse(raw.trim()).map_err(|err| OagwError::InvalidEndpoint {
            reason: format!("'{raw}' is not a valid endpoint URL: {err}"),
        })?;
        let scheme = EndpointScheme::parse(parsed.scheme())?;
        let host = parsed
            .host_str()
            .ok_or_else(|| OagwError::InvalidEndpoint {
                reason: format!("'{raw}' carries no host"),
            })?;
        Self::new(scheme, host, parsed.port())
    }

    /// Endpoint scheme.
    #[must_use]
    pub const fn scheme(&self) -> EndpointScheme {
        self.scheme
    }

    /// Normalized host (ASCII lowercase, no trailing dot, no brackets).
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Explicitly configured port, if any.
    #[must_use]
    pub const fn port(&self) -> Option<u16> {
        self.port
    }

    /// Port used for egress: the configured port or the scheme default.
    #[must_use]
    pub const fn effective_port(&self) -> u16 {
        match self.port {
            Some(port) => port,
            None => self.scheme.default_port(),
        }
    }

    /// `true` when the host is an IP address (an endpoint type that requires
    /// an explicit alias, `docs/PRD.md` §5.5).
    #[must_use]
    pub fn is_ip_endpoint(&self) -> bool {
        self.host.parse::<IpAddr>().is_ok()
    }

    /// `host:port` authority used for the `Host`/`:authority` pseudo header.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.is_ipv6() {
            format!("[{}]:{}", self.host, self.effective_port())
        } else {
            format!("{}:{}", self.host, self.effective_port())
        }
    }

    /// Base URL of the endpoint (`<scheme>://<authority>`).
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("{}://{}", self.scheme, self.authority())
    }

    /// `true` when the host is an IPv6 address (needs brackets in a URL).
    fn is_ipv6(&self) -> bool {
        self.host
            .parse::<IpAddr>()
            .is_ok_and(|ip| matches!(ip, IpAddr::V6(_)))
    }
}

/// The load-balanced endpoint pool of an upstream (`docs/DESIGN.md` §3.1
/// `ServerConfig`).
///
/// Pool invariants (`docs/PRD.md` §5.5 "Multi-Endpoint Pooling"): at least
/// one endpoint, and all endpoints share one scheme and one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// Builds a pool from its endpoints.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the pool is empty and
    /// [`OagwError::EndpointPoolMismatch`] when the endpoints mix schemes or
    /// ports.
    pub fn new(endpoints: Vec<Endpoint>) -> Result<Self, OagwError> {
        let Some(first) = endpoints.first() else {
            return Err(OagwError::Validation {
                detail: String::from(
                    "at least one endpoint is required (server.endpoints.minItems = 1)",
                ),
            });
        };
        for (idx, endpoint) in endpoints.iter().enumerate().skip(1) {
            if endpoint.scheme() != first.scheme() {
                return Err(OagwError::EndpointPoolMismatch {
                    reason: format!(
                        "endpoints must share one scheme: endpoints[0] uses '{}', endpoints[{idx}] uses '{}'",
                        first.scheme(),
                        endpoint.scheme()
                    ),
                });
            }
            if endpoint.effective_port() != first.effective_port() {
                return Err(OagwError::EndpointPoolMismatch {
                    reason: format!(
                        "endpoints must share one port: endpoints[0] uses {}, endpoints[{idx}] uses {}",
                        first.effective_port(),
                        endpoint.effective_port()
                    ),
                });
            }
        }
        Ok(Self { endpoints })
    }

    /// Endpoints of the pool, in configuration order.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// Primary endpoint (the first configured one).
    ///
    /// # Panics
    ///
    /// Never: a pool always holds at least one endpoint.
    #[must_use]
    pub fn primary(&self) -> &Endpoint {
        &self.endpoints[0]
    }
}

/// Protocol used to reach an upstream (`docs/schemas/upstream.v1.schema.json`
/// `protocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Protocol {
    /// Plain HTTP (and SSE / WebSocket upgrades).
    #[default]
    Http,
    /// gRPC (match keys are planned, no gRPC proxy path is reachable yet).
    Grpc,
}

impl Protocol {
    /// GTS identifier of the protocol.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }

    /// Short protocol name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Grpc => "grpc",
        }
    }

    /// Parses either the GTS identifier or the short protocol name.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown protocol.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw {
            PROTOCOL_HTTP | "http" => Ok(Self::Http),
            PROTOCOL_GRPC | "grpc" => Ok(Self::Grpc),
            _ => Err(OagwError::Validation {
                detail: format!("unknown upstream protocol '{raw}'"),
            }),
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Sharing mode of a hierarchical configuration slot
/// (`docs/PRD.md` §5.5 "Hierarchical Configuration Override").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; a descendant may override it.
    Inherit,
    /// Visible; a descendant cannot override it.
    Enforce,
}

impl SharingMode {
    /// `true` when descendants may see the value.
    #[must_use]
    pub const fn is_visible_to_descendants(self) -> bool {
        !matches!(self, Self::Private)
    }

    /// `true` when descendants may replace the value.
    #[must_use]
    pub const fn allows_override(self) -> bool {
        matches!(self, Self::Inherit)
    }

    /// `true` when the value is a floor for descendants.
    #[must_use]
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::Enforce)
    }

    /// Parses a sharing-mode token.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown token.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "private" => Ok(Self::Private),
            "inherit" => Ok(Self::Inherit),
            "enforce" => Ok(Self::Enforce),
            _ => Err(OagwError::Validation {
                detail: format!(
                    "unknown sharing mode '{raw}' (expected private, inherit or enforce)"
                ),
            }),
        }
    }
}

/// Rate limit window (`docs/schemas/upstream.v1.schema.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RateLimitWindow {
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

impl RateLimitWindow {
    /// Window length in seconds.
    #[must_use]
    pub const fn duration_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained rate of a rate limit (`rate` tokens per `window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SustainedRate {
    /// Tokens replenished per window (minimum 1).
    pub rate: NonZeroU32,
    /// Length of the window.
    pub window: RateLimitWindow,
}

impl SustainedRate {
    /// Tokens replenished per second, used to compare limits configured in
    /// different windows.
    #[must_use]
    pub fn per_second(&self) -> u64 {
        u64::from(self.rate.get()) * self.window.duration_secs()
    }

    /// The stricter (lower) of two sustained rates
    /// (`docs/PRD.md` §5.5: descendants can only be stricter).
    #[must_use]
    pub fn stricter_of(self, other: Self) -> Self {
        if self.per_second() <= other.per_second() {
            self
        } else {
            other
        }
    }
}

/// Burst capacity of a rate limit (defaults to the sustained rate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BurstCapacity {
    /// Bucket capacity (minimum 1).
    pub capacity: NonZeroU32,
}

/// Rate limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RateLimitAlgorithm {
    /// Allows bursts (default).
    #[default]
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Scope of the rate limit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RateLimitScope {
    /// One counter for the whole gear.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RateLimitStrategy {
    /// Reject with `429 RateLimitExceeded` and `Retry-After` (default).
    #[default]
    Reject,
    /// Queue the request within a bounded capacity.
    Queue,
    /// Serve the request with reduced functionality.
    Degrade,
}

/// Rate limit configuration of an upstream or a route
/// (`docs/schemas/upstream.v1.schema.json` `rate_limit`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Token bucket or sliding window.
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (required by the schema).
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to the sustained rate.
    pub burst: Option<BurstCapacity>,
    /// Scope of the counters.
    pub scope: RateLimitScope,
    /// Behaviour when the limit is exceeded.
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request (minimum 1).
    pub cost: NonZeroU32,
}

impl RateLimitConfig {
    /// Effective rate limit across one ancestor → descendant hop of the
    /// configuration hierarchy (`docs/PRD.md` §5.5).
    ///
    /// * `private` ancestor: the ancestor limit is not visible, the
    ///   descendant value applies;
    /// * `inherit`: the descendant value wins when specified, otherwise the
    ///   ancestor's is inherited;
    /// * `enforce`: the stricter (lower) of the two applies, so a descendant
    ///   can only tighten an enforced limit.
    #[must_use]
    pub fn effective(parent: Option<&Self>, child: Option<&Self>) -> Option<Self> {
        let Some(parent) = parent else {
            return child.cloned();
        };
        if !parent.sharing.is_visible_to_descendants() {
            return child.cloned();
        }
        match child {
            None => Some(parent.clone()),
            Some(child) if parent.sharing.is_enforced() => Some(parent.merge_enforced(child)),
            Some(_) => child.cloned(),
        }
    }

    /// Stricter combination of an enforced ancestor limit with a descendant
    /// override.
    fn merge_enforced(&self, child: &Self) -> Self {
        Self {
            sharing: child.sharing,
            algorithm: child.algorithm,
            sustained: self.sustained.stricter_of(child.sustained),
            burst: strictest_burst(self.burst, child.burst),
            scope: child.scope,
            strategy: child.strategy,
            cost: child.cost,
        }
    }
}

/// Strictest of two optional burst capacities.
fn strictest_burst(
    parent: Option<BurstCapacity>,
    child: Option<BurstCapacity>,
) -> Option<BurstCapacity> {
    match (parent, child) {
        (Some(parent), Some(child)) => Some(if parent.capacity.get() <= child.capacity.get() {
            parent
        } else {
            child
        }),
        (Some(parent), None) | (None, Some(parent)) => Some(parent),
        (None, None) => None,
    }
}

/// CORS configuration of an upstream or a route
/// (`docs/schemas/upstream.v1.schema.json` `cors`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled.
    pub enabled: bool,
    /// Allowed origins (`*` or exact origins).
    pub allowed_origins: Vec<AllowedOrigin>,
    /// Allowed HTTP methods.
    pub allowed_methods: Vec<HttpMethod>,
    /// Headers exposed to the browser beyond the CORS-safelisted ones.
    pub expose_headers: Vec<String>,
    /// Whether credentials (cookies, auth headers) are allowed.
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Effective CORS configuration across one ancestor → descendant hop.
    #[must_use]
    pub fn effective(parent: Option<&Self>, child: Option<&Self>) -> Option<Self> {
        match parent {
            None => child.cloned(),
            Some(parent) => resolve_slot(Some(parent), child, parent.sharing),
        }
    }

    /// Validates the CORS invariants of the schema: credentials require
    /// specific origins (`*` is forbidden together with
    /// `allow_credentials: true`).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the configuration combines
    /// credentials with the wildcard origin.
    pub fn validate(&self) -> Result<(), OagwError> {
        if !self.allow_credentials {
            return Ok(());
        }
        if self
            .allowed_origins
            .iter()
            .any(|origin| matches!(origin, AllowedOrigin::Any))
        {
            return Err(OagwError::Validation {
                detail: String::from(
                    "cors.allow_credentials requires specific origins; '*' is not allowed",
                ),
            });
        }
        Ok(())
    }
}

/// An entry of the CORS `allowed_origins` list.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AllowedOrigin {
    /// `*` — any origin.
    Any,
    /// An exact origin (`scheme://host[:port]`, no path).
    Exact(String),
}

impl AllowedOrigin {
    /// Parses an origin entry.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the value is neither `*` nor a
    /// bare origin URI (no path, query or fragment).
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        if raw == "*" {
            return Ok(Self::Any);
        }
        let parsed = url::Url::parse(raw).map_err(|err| OagwError::Validation {
            detail: format!("'{raw}' is not a valid CORS origin: {err}"),
        })?;
        let has_host = parsed.host_str().is_some_and(|host| !host.is_empty());
        let is_bare = matches!(parsed.path(), "" | "/")
            && parsed.query().is_none()
            && parsed.fragment().is_none();
        if !has_host || !is_bare {
            return Err(OagwError::Validation {
                detail: format!("'{raw}' is not an origin (expected scheme://host[:port])"),
            });
        }
        Ok(Self::Exact(raw.to_ascii_lowercase()))
    }
}

/// HTTP methods used by route matching and CORS configuration
/// (`docs/schemas/route.v1.schema.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// `HEAD` (CORS only).
    Head,
    /// `OPTIONS` (CORS preflight).
    Options,
}

impl HttpMethod {
    /// Method token.
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

    /// `true` for the methods a route match may list
    /// (`docs/schemas/route.v1.schema.json` `http_match.methods`).
    #[must_use]
    pub const fn is_route_match_method(self) -> bool {
        matches!(
            self,
            Self::Get | Self::Post | Self::Put | Self::Patch | Self::Delete
        )
    }

    /// Parses a method token.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown method.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            "PUT" => Ok(Self::Put),
            "PATCH" => Ok(Self::Patch),
            "DELETE" => Ok(Self::Delete),
            "HEAD" => Ok(Self::Head),
            "OPTIONS" => Ok(Self::Options),
            _ => Err(OagwError::Validation {
                detail: format!("unknown HTTP method '{raw}'"),
            }),
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Which inbound request headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HeaderPassthrough {
    /// Forward no inbound header (default).
    #[default]
    None,
    /// Forward only the allowlisted headers.
    Allowlist,
    /// Forward all inbound headers.
    All,
}

/// Request header transformation rules
/// (`docs/schemas/upstream.v1.schema.json` `headers.request`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite when present).
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    pub passthrough: HeaderPassthrough,
    /// Headers forwarded when `passthrough` is [`HeaderPassthrough::Allowlist`].
    pub passthrough_allowlist: Vec<String>,
}

/// Response header rules
/// (`docs/schemas/upstream.v1.schema.json` `headers.response`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    pub add: BTreeMap<String, String>,
    /// Headers stripped from the upstream response.
    pub remove: Vec<String>,
}

/// Header transformation rules of an upstream
/// (`docs/schemas/upstream.v1.schema.json` `headers`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadersConfig {
    /// Rules applied to the request towards the upstream.
    pub request: Option<RequestHeaderRules>,
    /// Rules applied to the response towards the client.
    pub response: Option<ResponseHeaderRules>,
}

impl HeadersConfig {
    /// Validates every configured header name.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when a header name is not a valid
    /// RFC 9110 token.
    pub fn validate(&self) -> Result<(), OagwError> {
        if let Some(request) = &self.request {
            validate_header_names(request.set.keys())?;
            validate_header_names(request.add.keys())?;
            validate_header_names(request.remove.iter())?;
            validate_header_names(request.passthrough_allowlist.iter())?;
        }
        if let Some(response) = &self.response {
            validate_header_names(response.set.keys())?;
            validate_header_names(response.add.keys())?;
            validate_header_names(response.remove.iter())?;
        }
        Ok(())
    }
}

/// Validates a set of header names.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for the first invalid header name.
fn validate_header_names<'a, I>(names: I) -> Result<(), OagwError>
where
    I: IntoIterator<Item = &'a String>,
{
    for name in names {
        if !is_valid_header_name(name) {
            return Err(OagwError::Validation {
                detail: format!("'{name}' is not a valid HTTP header name"),
            });
        }
    }
    Ok(())
}

/// RFC 9110 field-name check: printable ASCII without separators.
fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// A `cred://` URI reference.
///
/// OAGW never stores or logs secret material
/// (`cpt-cf-oagw-principle-cred-isolation`): it only carries references into
/// the credential store, and this type refuses to render itself in logs or
/// problem bodies.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SecretRef {
    value: String,
}

impl SecretRef {
    /// Scheme prefix of a credential-store reference.
    pub const SCHEME: &'static str = "cred://";

    /// Parses a `cred://` reference.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the value does not use the
    /// `cred://` scheme or carries no path.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        let value = raw.trim();
        if !value.starts_with(Self::SCHEME) || value.len() <= Self::SCHEME.len() {
            return Err(OagwError::Validation {
                detail: String::from("credential references must use the 'cred://<path>' form"),
            });
        }
        Ok(Self {
            value: value.to_owned(),
        })
    }

    /// The reference, for handing to the credential store.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Redacted form, safe for logs, metrics and problem bodies.
    #[must_use]
    pub fn redacted(&self) -> String {
        String::from("cred://***")
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Credential isolation: the reference itself never reaches a log.
        write!(f, "SecretRef(***)")
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.redacted())
    }
}

/// Authentication plugin configuration of an upstream
/// (`docs/schemas/upstream.v1.schema.json` `auth`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AuthConfig {
    /// Hierarchical sharing mode of the credentials.
    pub sharing: SharingMode,
    /// Auth plugin bound to the upstream.
    pub plugin: Option<PluginRef>,
    /// Plugin configuration; may carry `cred://` references under any key.
    pub config: serde_json::Value,
}

impl AuthConfig {
    /// Extracts a credential reference from the plugin configuration.
    ///
    /// `None` when the key is absent or holds a plain value: only a
    /// `cred://`-prefixed value is treated as a credential reference
    /// (`cpt-cf-oagw-principle-cred-isolation`).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the value under `key` looks
    /// like a `cred://` reference but is malformed.
    pub fn secret_ref(&self, key: &str) -> Result<Option<SecretRef>, OagwError> {
        let Some(raw) = self.config.get(key).and_then(serde_json::Value::as_str) else {
            return Ok(None);
        };
        if !raw.starts_with(SecretRef::SCHEME) {
            return Ok(None);
        }
        SecretRef::parse(raw).map(Some)
    }
}

/// Ordered plugin chain of an upstream or a route
/// (`docs/schemas/upstream.v1.schema.json` `plugins`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginChain {
    /// Hierarchical sharing mode of the chain.
    pub sharing: SharingMode,
    /// Plugins in execution order.
    pub items: Vec<PluginRef>,
}

impl PluginChain {
    /// Effective chain across one ancestor → descendant hop
    /// (`docs/PRD.md` §5.5): a descendant appends its plugins; inherited
    /// entries are never removed.
    #[must_use]
    pub fn effective(parent: Option<&Self>, child: Option<&Self>) -> Option<Self> {
        let Some(parent) = parent else {
            return child.cloned();
        };
        if !parent.sharing.is_visible_to_descendants() {
            return child.cloned();
        }
        match child {
            None => Some(parent.clone()),
            Some(child) => Some(parent.append(child)),
        }
    }

    /// Appends the plugins of `child` that are not already present.
    #[must_use]
    fn append(&self, child: &Self) -> Self {
        let mut items = self.items.clone();
        for plugin in &child.items {
            if !items.contains(plugin) {
                items.push(plugin.clone());
            }
        }
        Self {
            sharing: child.sharing,
            items,
        }
    }
}

/// Inputs of a new [`Upstream`], validated by [`Upstream::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamSpec {
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Requested alias, when the caller supplied one.
    pub alias: Option<Alias>,
    /// Protocol used to reach the upstream.
    pub protocol: Protocol,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Auth plugin configuration.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    pub plugins: Option<PluginChain>,
    /// Rate limit configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Flat discovery tags (add-only across the hierarchy).
    pub tags: Vec<String>,
}

/// Tenant-scoped root configuration object representing an external service
/// (`docs/DESIGN.md` §3.1 `Upstream`, GTS type
/// `gts.cf.core.oagw.upstream.v1~`).
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Routing identifier, derived or explicitly supplied.
    pub alias: Alias,
    /// Protocol used to reach the upstream.
    pub protocol: Protocol,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Auth plugin configuration.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    pub plugins: Option<PluginChain>,
    /// Rate limit configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Flat discovery tags.
    pub tags: Vec<String>,
}

impl Upstream {
    /// Assembles an upstream, enforcing the alias contract and the
    /// configuration invariants.
    ///
    /// # Errors
    ///
    /// Returns the alias, endpoint, tag, header or CORS validation errors of
    /// the parts (see [`resolve_alias`], [`Alias::parse`],
    /// [`ServerConfig::new`], [`CorsConfig::validate`],
    /// [`HeadersConfig::validate`]).
    pub fn new(id: uuid::Uuid, spec: &UpstreamSpec) -> Result<Self, OagwError> {
        let alias = resolve_alias(spec.server.endpoints(), spec.alias.as_ref())?;
        validate_tags(&spec.tags)?;
        if let Some(cors) = &spec.cors {
            cors.validate()?;
        }
        if let Some(headers) = &spec.headers {
            headers.validate()?;
        }
        Ok(Self {
            id,
            tenant_id: spec.tenant_id,
            alias,
            protocol: spec.protocol,
            enabled: spec.enabled,
            server: spec.server.clone(),
            auth: spec.auth.clone(),
            headers: spec.headers.clone(),
            plugins: spec.plugins.clone(),
            rate_limit: spec.rate_limit.clone(),
            cors: spec.cors.clone(),
            tags: spec.tags.clone(),
        })
    }

    /// `true` when the upstream accepts proxy traffic.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Validates the flat tag pattern `^[a-z0-9_-]+$`.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for a tag outside the pattern.
pub fn validate_tags(tags: &[String]) -> Result<(), OagwError> {
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            });
        if !valid {
            return Err(OagwError::Validation {
                detail: format!("tag '{tag}' must match ^[a-z0-9_-]+$"),
            });
        }
    }
    Ok(())
}

/// Normalizes and validates an endpoint host: ASCII lowercase, trailing dots
/// stripped, IPv4/IPv6 or RFC 1123 hostname.
fn normalize_host(raw: &str) -> Result<String, OagwError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(OagwError::InvalidEndpoint {
            reason: String::from("the endpoint host must not be empty"),
        });
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    let host = unbracketed.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(OagwError::InvalidEndpoint {
            reason: format!("'{raw}' carries no host"),
        });
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(host);
    }
    validate_hostname(&host, raw)?;
    Ok(host)
}

/// Validates an RFC 1123 hostname.
///
/// # Errors
///
/// Returns [`OagwError::InvalidEndpoint`] when a label is empty, too long, or
/// carries a character outside the hostname alphabet.
fn validate_hostname(host: &str, raw: &str) -> Result<(), OagwError> {
    if host.len() > MAX_HOST_LEN {
        return Err(OagwError::InvalidEndpoint {
            reason: format!("host '{host}' exceeds {MAX_HOST_LEN} characters"),
        });
    }
    if host.contains(':') {
        return Err(OagwError::InvalidEndpoint {
            reason: format!("host '{host}' must not carry a port"),
        });
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(OagwError::InvalidEndpoint {
                reason: format!("host '{host}' carries an empty label"),
            });
        }
        if label.len() > MAX_HOST_LABEL_LEN {
            return Err(OagwError::InvalidEndpoint {
                reason: format!("label '{label}' exceeds {MAX_HOST_LABEL_LEN} characters"),
            });
        }
        let valid = label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-');
        if !valid {
            return Err(OagwError::InvalidEndpoint {
                reason: format!("'{raw}' is not a valid hostname or IP address"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "upstream_tests.rs"]
mod upstream_tests;
