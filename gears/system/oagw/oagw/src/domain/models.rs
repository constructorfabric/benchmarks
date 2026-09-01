//! Domain models for the OAGW control plane.
//!
//! These mirror the authoritative JSON Schemas
//! (`docs/schemas/upstream.v1.schema.json`, `docs/schemas/route.v1.schema.json`)
//! as Rust structs with serde defaults, plus the (server-owned) plugin record.
//! Validation rules (alias derivation, required fields, immutability) live in
//! `crate::domain::service` and `crate::api::rest::dto`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// GTS identifier of an upstream's protocol.
pub const PROTOCOL_HTTP_V1: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS identifier of a gRPC upstream protocol.
pub const PROTOCOL_GRPC_V1: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Standard default ports per scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Https,
    Wss,
    Wt,
    Grpc,
}

impl Scheme {
    /// Match a wire scheme string (unknown schemes are rejected at validation).
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "https" => Some(Self::Https),
            "wss" => Some(Self::Wss),
            "wt" => Some(Self::Wt),
            "grpc" => Some(Self::Grpc),
            _ => None,
        }
    }

    /// Per-scheme default port.
    #[must_use]
    pub fn default_port(self) -> u16 {
        let _ = self;
        443
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

impl Serialize for Scheme {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Scheme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Scheme::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid scheme '{s}'")))
    }
}

/// Header transformation rules shared by request and response directions.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HeaderOps {
    /// Headers to set (overwrite if exists).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, allow duplicates).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to remove.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Inbound request-header forwarding policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward none of the inbound headers.
    #[default]
    None,
    /// Forward only the headers named in `passthrough_allowlist`.
    Allowlist,
    /// Forward all inbound headers.
    All,
}

/// Request-direction header rules (includes the passthrough policy).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RequestHeadersConfig {
    /// Headers to set on outbound requests.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to add on outbound requests.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from inbound requests.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Headers to forward when `passthrough == allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Full headers configuration of an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct HeadersConfig {
    /// Request-direction rules.
    #[serde(default)]
    pub request: RequestHeadersConfig,
    /// Response-direction rules.
    #[serde(default)]
    pub response: HeaderOps,
}

/// A single upstream endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Endpoint {
    #[serde(default = "default_scheme")]
    pub scheme: Scheme,
    pub host: String,
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
}

fn default_scheme() -> Scheme {
    Scheme::Https
}

fn default_endpoint_port() -> u16 {
    443
}

/// The `server` section of an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    /// One or more upstream endpoints.
    #[serde(default = "Vec::new")]
    pub endpoints: Vec<Endpoint>,
}

/// Sharing mode for hierarchical config composition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendant tenants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override (ancestor wins).
    Enforce,
}

/// Auth plugin configuration attached to an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin type.
    pub r#type: String,
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration object.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Plugin chain attached to an upstream or route.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    /// Builtin plugins by GTS ID; custom plugins by UUID.
    #[serde(default)]
    pub items: Vec<String>,
}

/// Rate-limiting window.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    /// Number of seconds in the window.
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

/// Rate-limit scope for counters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// Process-wide.
    Global,
    /// Per-tenant.
    #[default]
    Tenant,
    /// Per-user (from the security context).
    User,
    /// Per-source IP.
    Ip,
    /// Per-route.
    Route,
}

/// Rate-limit strategy when the limit is exceeded.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    /// Queue (honored as `reject` in the MVP; see [`crate::infra::ratelimit`]).
    Queue,
    /// Degrade (honored as `reject` in the MVP).
    Degrade,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Rate-limiting configuration (upstream or route).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (tokens per window).
    pub sustained: SustainedRate,
    /// Burst capacity (defaults to `sustained.rate`).
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
}

fn default_cost() -> u64 {
    1
}

/// Sustained rate specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SustainedRate {
    pub rate: u64,
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst specification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BurstConfig {
    pub capacity: u64,
}

/// CORS configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

/// An upstream service definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Upstream {
    /// System-generated UUID.
    #[serde(default = "gen_uuid")]
    pub id: Uuid,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing alias (auto-derived for hostname endpoints; required for IP).
    #[serde(default)]
    pub alias: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: String,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: HeadersConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

fn gen_uuid() -> Uuid {
    Uuid::new_v4()
}

fn default_true() -> bool {
    true
}

/// HTTP route matching rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpMatch {
    pub methods: Vec<String>,
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How a proxy `path_suffix` is treated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests that carry a path suffix.
    Disabled,
    /// Append the suffix to the configured path.
    #[default]
    Append,
}

/// gRPC route matching rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Route matching rules — exactly one of http|grpc must be present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MatchRule {
    #[serde(default)]
    pub http: Option<HttpMatch>,
    #[serde(default)]
    pub grpc: Option<GrpcMatch>,
}

/// A route definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    #[serde(default = "gen_uuid")]
    pub id: Uuid,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Reference to the upstream for this route (immutable after creation).
    pub upstream_id: Uuid,
    #[serde(default)]
    pub r#match: Option<MatchRule>,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

impl Route {
    /// HTTP match rules, if this route is HTTP-scoped.
    #[must_use]
    pub fn http_match(&self) -> Option<&HttpMatch> {
        self.r#match.as_ref().and_then(|m| m.http.as_ref())
    }
}

/// Kind of custom plugin the control plane may store.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Starlark custom plugin. Not executable in the MVP (no sandbox); stored
    /// for management compatibility so plugin CRUD and delete-in-use 409
    /// semantics remain testable.
    #[default]
    Starlark,
}

/// A custom (tenant-defined) plugin record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginRecord {
    /// Deserializes to the nil UUID when omitted so the control plane can
    /// reject client-supplied ids ("id is system-generated"). Unlike
    /// `Upstream`/`Route`, the id is *not* generated at deserialization.
    #[serde(default)]
    pub id: Uuid,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: PluginKind,
    /// Starlark source (required for `kind: starlark`).
    #[serde(default)]
    pub source: String,
    /// Whether the plugin is active (bindable).
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Reference detail for the plugin-delete 409 body (ADR 0001): which
/// upstreams and routes currently bind this plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ReferencedBy {
    #[serde(default)]
    pub upstreams: Vec<String>,
    #[serde(default)]
    pub routes: Vec<String>,
}

impl ReferencedBy {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

/// Key used by the in-memory stores for uniqueness bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateKey {
    /// `(tenant_id, alias)` pair.
    Alias(String, String),
    /// Route id already exists.
    Route(String),
    /// Plugin id already exists.
    Plugin(String),
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn upstream_deserializes_with_schema_defaults() {
        let json = r#"{
            "server": {
                "endpoints": [{ "scheme": "https", "host": "api.example.com" }]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }"#;
        let u: Upstream = serde_json::from_str(json).unwrap();
        assert!(u.enabled);
        assert_eq!(u.server.endpoints[0].port, 443);
        assert_eq!(u.server.endpoints[0].scheme, Scheme::Https);
        assert_eq!(u.rate_limit, None);
        assert_eq!(u.headers.request.passthrough, PassthroughMode::None);
    }

    #[test]
    fn route_deserializes_with_schema_defaults() {
        let json = r#"{
            "upstream_id": "6f0d7a2e-4a0a-4b0f-8d5a-2e2f0b1a3c4d",
            "match": {
                "http": { "methods": ["GET"], "path": "/v1/chat" }
            }
        }"#;
        let r: Route = serde_json::from_str(json).unwrap();
        let m = r.http_match().expect("http match present");
        assert_eq!(m.path_suffix_mode, PathSuffixMode::Append);
        assert!(r.plugins.items.is_empty());
        // Routes default to enabled (PRD cpt-cf-oagw-fr-enable-disable).
        assert!(r.enabled);
    }

    #[test]
    fn invalid_endpoint_scheme_is_rejected() {
        let json = r#"{
            "server": { "endpoints": [{ "scheme": "ftp", "host": "x.com" }] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }"#;
        assert!(serde_json::from_str::<Upstream>(json).is_err());
    }

    #[test]
    fn scheme_defaults_and_roundtrip() {
        assert_eq!(Scheme::Https.default_port(), 443);
        assert_eq!(Scheme::Https.as_str(), "https");
        let ser = serde_json::to_string(&Scheme::Wss).unwrap();
        assert_eq!(ser, "\"wss\"");
        assert_eq!(Scheme::parse("wt"), Some(Scheme::Wt));
        assert_eq!(Scheme::parse("nope"), None);
    }
}
