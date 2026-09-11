// Updated: 2026-09-01 by Constructor Tech
//! Domain models for the OAGW Control Plane.
//!
//! These are the authoritative shapes: they are what the REST layer
//! deserializes, what the storage layer persists in memory, and what the Data
//! Plane consumes when it resolves a configuration. Field names and enums
//! mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`.
//!
//! Two deliberate divergences from the literal schemas, both documented where
//! they happen:
//!
//! * [`Scheme`] also admits `http`. The schema enumerates only the TLS
//!   family, which describes the *default* posture; `allow_http_upstream` is
//!   what lifts it, and rejecting the value at create time would reject a
//!   legal request. Whether a plaintext connection is actually made is decided
//!   at proxy time.
//! * [`RequestHeaderRules::passthrough`] follows the schema's
//!   `"default": "none"`: absent, the gateway forwards no inbound header of
//!   its own accord. `content-type` travels regardless, because DESIGN makes
//!   the gateway responsible for the well-known headers and a proxied body
//!   without one is not a message; an explicit `set` or `remove` still wins.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::{DomainError, IssueCollector};

// ── Enums ───────────────────────────────────────────────────────────────────

/// Endpoint transport scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(request, response)]
pub enum Scheme {
    /// Plaintext HTTP. Only dialled when `allow_http_upstream` is set.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport over TLS.
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl Scheme {
    /// The port used when the endpoint does not state one.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether the scheme implies TLS on the wire.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }

    /// The scheme used when actually dialling: everything non-gRPC speaks
    /// HTTP/1.1 on the wire.
    #[must_use]
    pub fn wire_scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            _ => "https",
        }
    }
}

impl std::fmt::Display for Scheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.wire_scheme())
    }
}

/// Wire protocol spoken to the upstream.
///
/// Serializes to the GTS identifier form the upstream schema pins rather than
/// to the bare variant name, so it carries its own serde impls instead of the
/// derive the `api_dto` attribute would add.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, utoipa::ToSchema)]
#[schema(rename_all = "lowercase")]
#[derive(Default)]
pub enum Protocol {
    #[default]
    Http,
    Grpc,
}

impl toolkit::api::api_dto::RequestApiDto for Protocol {}
impl toolkit::api::api_dto::ResponseApiDto for Protocol {}

impl Protocol {
    #[must_use]
    pub fn as_gts_id(self) -> &'static str {
        match self {
            Self::Http => crate::gts::PROTOCOL_HTTP,
            Self::Grpc => crate::gts::PROTOCOL_GRPC,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "http" => Some(Self::Http),
            "grpc" => Some(Self::Grpc),
            other if other == crate::gts::PROTOCOL_HTTP => Some(Self::Http),
            other if other == crate::gts::PROTOCOL_GRPC => Some(Self::Grpc),
            _ => None,
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Http => "http",
            Self::Grpc => "grpc",
        })
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown protocol '{s}': expected http or grpc (GTS identifier form)"
            ))
        })
    }
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_gts_id())
    }
}

/// Hierarchical sharing posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum SharingMode {
    /// Visible only to the owning tenant.
    #[default]
    Private,
    /// Visible to descendants, who may override it.
    Inherit,
    /// Visible to descendants, who may not override it.
    Enforce,
}

/// Which inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(request, response)]
pub enum PassthroughMode {
    None,
    Allowlist,
    All,
}

/// HTTP method accepted by a route's `match.http.methods`.
///
/// The route schema pins the canonical token form (`GET`, `POST`, …), so each
/// variant is renamed explicitly — the `api_dto` derive would otherwise emit
/// `snake_case` and reject a legal request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(request, response)]
pub enum HttpMethod {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
    #[serde(rename = "PUT")]
    Put,
    #[serde(rename = "DELETE")]
    Delete,
    #[serde(rename = "PATCH")]
    Patch,
}

impl HttpMethod {
    /// Parse an inbound request method. `HEAD` is treated as `GET` (it is the
    /// same resource), `OPTIONS` is handled by the CORS layer.
    #[must_use]
    pub fn parse(m: &http::Method) -> Option<Self> {
        match *m {
            http::Method::GET => Some(Self::Get),
            http::Method::POST => Some(Self::Post),
            http::Method::PUT => Some(Self::Put),
            http::Method::DELETE => Some(Self::Delete),
            http::Method::PATCH => Some(Self::Patch),
            _ => None,
        }
    }

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
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the suffix after `match.http.path` is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum PathSuffixMode {
    /// A suffix is rejected with a `ValidationError`.
    Disabled,
    /// The suffix is appended to the matched path.
    #[default]
    Append,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Counter key scope for a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitScope {
    #[default]
    Tenant,
    User,
    Ip,
    Route,
    Global,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Time window of the sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    #[must_use]
    pub fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

// ── Endpoint / server ───────────────────────────────────────────────────────

/// One dialable upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(request, response)]
pub struct Endpoint {
    #[serde(default = "default_https")]
    pub scheme: Scheme,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_https() -> Scheme {
    Scheme::Https
}

impl Endpoint {
    /// The effective port: explicit, else the scheme's standard port.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// Lowercased host with surrounding brackets stripped (IPv6 literals are
    /// stored bare; the port is always separate).
    #[must_use]
    pub fn normalized_host(&self) -> String {
        let h = self
            .host
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']');
        h.trim_end_matches('.').to_ascii_lowercase()
    }

    /// `host:port` form used for display and for alias derivation.
    #[must_use]
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.normalized_host(), self.effective_port())
    }

    /// `scheme://host:port` used when dialling.
    #[must_use]
    pub fn authority(&self) -> String {
        format!("{}:{}", self.normalized_host(), self.effective_port())
    }

    /// `Host` header value to send upstream.
    ///
    /// An IPv6 literal is bracketed, per RFC 3986 §3.2.2 — the colons are
    /// ambiguous without them.
    #[must_use]
    pub fn host_header(&self) -> String {
        let host = self.normalized_host();
        let port = self.effective_port();
        let default = self.scheme.default_port();
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        if port == default {
            host
        } else {
            format!("{host}:{port}")
        }
    }
}

/// Group of endpoints forming one logical upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct ServerConfig {
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

// ── Auth ────────────────────────────────────────────────────────────────────

/// Upstream-level authentication declaration.
///
/// `type` is the *plugin type* — either a builtin GTS identifier or the name
/// of a registered custom plugin. Credentials are referenced by
/// `config.<...>_ref` and resolved from the credstore at request time; they
/// are never persisted by OAGW and never logged.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct AuthConfig {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config: Option<serde_json::Value>,
}

impl AuthConfig {
    #[must_use]
    pub fn sharing(&self) -> SharingMode {
        self.sharing.unwrap_or(SharingMode::Private)
    }

    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        self.config.as_ref().unwrap_or(&serde_json::Value::Null)
    }
}

// ── Headers ─────────────────────────────────────────────────────────────────

/// Request-side header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct RequestHeaderRules {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward. Absent is `none`: the gateway
    /// forwards what it is told to. `content-type` is carried regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<PassthroughMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct ResponseHeaderRules {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub set: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(value_type = Object)]
    pub add: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation rules for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeaderRules,
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

// ── Plugins ─────────────────────────────────────────────────────────────────

/// A reference to a plugin in an upstream or route plugin chain.
///
/// Builtin plugins are referenced by GTS identifier, custom plugins by UUID.
/// Both accept an inline `config` object, which overrides the plugin's own
/// stored configuration for that binding.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(untagged)]
pub enum PluginItem {
    /// GTS identifier or UUID string.
    Reference(String),
    /// Inline plugin definition.
    Inline(InlinePlugin),
}

/// Inline plugin definition, accepted wherever a plugin reference is.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct InlinePlugin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Builtin plugin type, e.g. `cf.core.oagw.required_headers.v1`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Plugin chain attached to an upstream or route.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginItem>,
}

// ── Rate limit ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct SustainedRate {
    pub rate: u64,
    #[serde(default)]
    pub window: RateWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct Burst {
    pub capacity: u64,
}

/// Token-bucket / sliding-window rate limit configuration (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_cost")]
    pub cost: u64,
}

fn default_cost() -> u64 {
    1
}

impl RateLimitConfig {
    /// Effective bucket capacity: `burst.capacity`, else the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> f64 {
        self.burst
            .map_or(self.sustained.rate as f64, |b| b.capacity as f64)
    }

    /// Permits per second.
    #[must_use]
    pub fn rate_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.secs() as f64
    }
}

// ── CORS ────────────────────────────────────────────────────────────────────

/// The methods a CORS configuration that names none of its own allows.
///
/// Held in a `static` so `CorsConfig::methods` can return a `&[String]` that
/// outlives the call without allocating on every read.
fn default_methods() -> &'static [String] {
    static METHODS: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| ["GET", "POST"].iter().map(|m| (*m).to_owned()).collect());
    &METHODS
}

/// Cross-origin configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_credentials: Option<bool>,
}

impl CorsConfig {
    /// Whether this upstream actually enforces a CORS policy.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn credentials(&self) -> bool {
        self.allow_credentials.unwrap_or(false)
    }

    #[must_use]
    pub fn methods(&self) -> &[String] {
        if self.allowed_methods.is_empty() {
            // A configuration that names nothing allows the two safe methods;
            // the default lives in a `static` so the reference is `'static`.
            default_methods()
        } else {
            &self.allowed_methods
        }
    }
}

// ── Upstream ────────────────────────────────────────────────────────────────

/// An upstream service: where to send traffic and how.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct Upstream {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

fn default_true() -> bool {
    true
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: Protocol::Http,
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: None,
            upstream_id: Uuid::nil(),
            r#match: RouteMatch::default(),
            enabled: true,
            priority: 0,
            tags: Vec::new(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

impl HeadersConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl PluginsConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sharing == SharingMode::Private && self.items.is_empty()
    }

    /// Whether any item in the chain binds the given custom plugin, by UUID or
    /// by a GTS instance identifier carrying that UUID.
    #[must_use]
    pub fn references(&self, plugin_id: Uuid) -> bool {
        self.items.iter().any(|item| match item {
            PluginItem::Reference(s) => {
                Uuid::parse_str(s) == Ok(plugin_id) || crate::gts::uuid_of(s) == Some(plugin_id)
            }
            PluginItem::Inline(inline) => {
                inline.id.as_deref().and_then(|s| Uuid::parse_str(s).ok()) == Some(plugin_id)
            }
        })
    }
}

/// Free-function form of [`PluginsConfig::references`], used by the
/// repositories, which only see the config struct.
#[must_use]
pub fn references_plugin(config: &PluginsConfig, plugin_id: Uuid) -> bool {
    config.references(plugin_id)
}

impl Upstream {
    /// The stored alias, if any. Derivation happens in the service layer.
    #[must_use]
    pub fn explicit_alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }
}

// ── Route ───────────────────────────────────────────────────────────────────

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Protocol-scoped match rules. Exactly one of `http` / `grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct RouteMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A route: which upstream serves a matched request, and with what overrides.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct Route {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    pub upstream_id: Uuid,
    pub r#match: RouteMatch,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "PluginsConfig::is_empty")]
    pub plugins: PluginsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Route {
    /// The path prefix this route matches, or `None` for a gRPC route.
    #[must_use]
    pub fn http_path(&self) -> Option<&str> {
        self.r#match.http.as_ref().map(|m| m.path.as_str())
    }
}

// ── Plugin (custom, Starlark) ───────────────────────────────────────────────

/// Plugin kind, mirroring the three plugin base types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[toolkit_macros::api_dto(request, response)]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            Self::Auth => crate::gts::AUTH_PLUGIN_TYPE,
            Self::Guard => crate::gts::GUARD_PLUGIN_TYPE,
            Self::Transform => crate::gts::TRANSFORM_PLUGIN_TYPE,
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auth" | "auth_plugin" | "authplugin" => Some(Self::Auth),
            "guard" | "guard_plugin" | "guardplugin" => Some(Self::Guard),
            "transform" | "transform_plugin" | "transformplugin" => Some(Self::Transform),
            other if other == crate::gts::AUTH_PLUGIN_TYPE => Some(Self::Auth),
            other if other == crate::gts::GUARD_PLUGIN_TYPE => Some(Self::Guard),
            other if other == crate::gts::TRANSFORM_PLUGIN_TYPE => Some(Self::Transform),
            _ => None,
        }
    }
}

/// A custom plugin definition. Immutable once created.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct Plugin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    #[serde(rename = "type")]
    pub kind: PluginKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config: Option<serde_json::Value>,
    /// Starlark source. Catalogued verbatim; never logged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

// ── Validation ──────────────────────────────────────────────────────────────

/// RFC 1123 hostname label / registrable-domain shape, plus IPv4 and bracketed
/// IPv6 literals.
#[must_use]
pub fn is_valid_host(raw: &str) -> bool {
    let host = raw.trim().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    // IPv6 literal (contains ':').
    if host.contains(':') {
        return host.parse::<std::net::Ipv6Addr>().is_ok();
    }
    // Bare IPv4.
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    // Hostname: letters, digits, hyphens and dots; labels 1-63 chars; no
    // leading/trailing hyphen; no empty label.
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Characters the alias pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` accepts.
#[must_use]
pub fn validate_alias_shape(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    let first = raw.chars().next().unwrap();
    let last = raw.chars().last().unwrap();
    let middle_ok = raw
        .chars()
        .skip(1)
        .take(raw.len().saturating_sub(2))
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | ':' | '-'));
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && (last.is_ascii_lowercase() || last.is_ascii_digit())
        && middle_ok
}

const TAG_PATTERN: fn(&str) -> bool = |t| {
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
};

/// Screen an upstream's endpoints against the configured SSRF posture.
///
/// Kept apart from [`validate_upstream`] on purpose: which values the field
/// accepts is a schema question, whether a connection may be dialled is a
/// policy question, and the policy is only known where the gear config is. An
/// upstream that names a loopback host is *well formed* even in a gear that
/// will never dial it.
///
/// # Errors
///
/// [`DomainError::Validation`] naming every endpoint the policy denies.
pub fn validate_upstream_ssrf(
    u: &Upstream,
    policy: &crate::config::SsrfPolicy,
) -> Result<(), DomainError> {
    if !policy.enabled {
        return Ok(());
    }
    let mut c = IssueCollector::new();
    for (i, ep) in u.server.endpoints.iter().enumerate() {
        let p = format!("server.endpoints[{i}].host");
        let host = ep.normalized_host();
        if is_valid_host(&host) && crate::infra::proxy::ssrf::is_denied_host(&host) {
            c.reject(
                true,
                &p,
                "loopback, link-local and private-network addresses are not permitted",
            );
        }
    }
    c.finish()
}

pub fn validate_upstream(u: &Upstream) -> Result<(), DomainError> {
    let mut c = IssueCollector::new();

    c.require(
        !u.server.endpoints.is_empty(),
        "server.endpoints",
        "at least one endpoint is required",
    );

    let mut seen = std::collections::BTreeSet::new();
    for (i, ep) in u.server.endpoints.iter().enumerate() {
        let p = format!("server.endpoints[{i}]");
        c.require(
            is_valid_host(&ep.host),
            &format!("{p}.host"),
            "must be a valid hostname or IP address",
        );
        if !is_valid_host(&ep.host) {
            continue;
        }
        let key = ep.host_port();
        c.reject(
            !seen.insert(key.clone()),
            &format!("{p}.host"),
            "duplicate endpoint {key}",
        );
        if let Some(port) = ep.port {
            c.require(
                port > 0,
                &format!("{p}.port"),
                "must be between 1 and 65535",
            );
        }
    }

    if let Some(alias) = &u.alias {
        c.require(
            validate_alias_shape(alias),
            "alias",
            "must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$",
        );
    }

    for (i, tag) in u.tags.iter().enumerate() {
        c.require(
            TAG_PATTERN(tag),
            &format!("tags[{i}]"),
            "must match ^[a-z0-9_-]+$",
        );
    }

    if let Some(auth) = &u.auth {
        c.require(
            auth.plugin_type
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty()),
            "auth.type",
            "an auth plugin type is required when auth is configured",
        );
    }

    for (i, item) in u.plugins.items.iter().enumerate() {
        if let PluginItem::Inline(inline) = item {
            c.require(
                inline
                    .plugin_type
                    .as_deref()
                    .is_some_and(|t| !t.trim().is_empty())
                    || inline.id.is_some(),
                &format!("plugins.items[{i}]"),
                "an inline plugin must declare either `type` or `id`",
            );
        } else if let PluginItem::Reference(r) = item {
            c.require(
                !r.trim().is_empty(),
                &format!("plugins.items[{i}]"),
                "plugin reference must not be empty",
            );
        }
    }

    if let Some(rl) = &u.rate_limit {
        validate_rate_limit(rl, "rate_limit", &mut c);
    }

    if let Some(cors) = &u.cors {
        validate_cors(cors, "cors", &mut c);
    }

    c.finish()
}

/// Validate a route for create/replace.
///
/// # Errors
///
/// [`DomainError::Validation`] collecting every offending field.
pub fn validate_route(r: &Route) -> Result<(), DomainError> {
    let mut c = IssueCollector::new();

    let http = r.r#match.http.as_ref();
    let grpc = r.r#match.grpc.as_ref();
    c.reject(
        http.is_none() && grpc.is_none(),
        "match",
        "exactly one of match.http or match.grpc is required",
    );
    c.reject(
        http.is_some() && grpc.is_some(),
        "match",
        "match.http and match.grpc are mutually exclusive",
    );

    if let Some(h) = http {
        c.require(
            !h.methods.is_empty(),
            "match.http.methods",
            "at least one method is required",
        );
        for (i, m) in h.methods.iter().enumerate() {
            let _ = m;
            let _ = i;
        }
        c.require(
            !h.path.is_empty(),
            "match.http.path",
            "path must not be empty",
        );
        c.require(
            h.path.starts_with('/') || h.path == "*",
            "match.http.path",
            "path must start with '/'",
        );
    }

    if let Some(g) = grpc {
        c.require(
            !g.service.is_empty(),
            "match.grpc.service",
            "service must not be empty",
        );
        c.require(
            !g.method.is_empty(),
            "match.grpc.method",
            "method must not be empty",
        );
    }

    for (i, item) in r.plugins.items.iter().enumerate() {
        match item {
            PluginItem::Reference(s) => {
                c.require(
                    !s.trim().is_empty(),
                    &format!("plugins.items[{i}]"),
                    "plugin reference must not be empty",
                );
            }
            PluginItem::Inline(inline) => {
                c.require(
                    inline
                        .plugin_type
                        .as_deref()
                        .is_some_and(|t| !t.trim().is_empty())
                        || inline.id.is_some(),
                    &format!("plugins.items[{i}]"),
                    "an inline plugin must declare either `type` or `id`",
                );
            }
        }
    }

    if let Some(rl) = &r.rate_limit {
        validate_rate_limit(rl, "rate_limit", &mut c);
    }

    if let Some(cors) = &r.cors {
        validate_cors(cors, "cors", &mut c);
    }

    c.finish()
}

/// Validate a custom plugin definition.
///
/// # Errors
///
/// [`DomainError::Validation`] collecting every offending field.
pub fn validate_plugin(p: &Plugin) -> Result<(), DomainError> {
    let mut c = IssueCollector::new();
    c.require(
        p.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
        "source",
        "Starlark source is required",
    );
    for (i, tag) in p.tags.iter().enumerate() {
        c.require(
            TAG_PATTERN(tag),
            &format!("tags[{i}]"),
            "must match ^[a-z0-9_-]+$",
        );
    }
    c.finish()
}

fn validate_rate_limit(rl: &RateLimitConfig, prefix: &str, c: &mut IssueCollector) {
    c.require(
        rl.sustained.rate >= 1,
        &format!("{prefix}.sustained.rate"),
        "must be at least 1",
    );
    c.require(
        rl.cost >= 1,
        &format!("{prefix}.cost"),
        "must be at least 1",
    );
    if let Some(b) = &rl.burst {
        c.require(
            b.capacity >= 1,
            &format!("{prefix}.burst.capacity"),
            "must be at least 1",
        );
    }
}

fn validate_cors(cors: &CorsConfig, prefix: &str, c: &mut IssueCollector) {
    if cors.credentials() && cors.allowed_origins.iter().any(|o| o == "*") {
        c.reject(
            true,
            &format!("{prefix}.allowed_origins"),
            "allow_credentials requires explicit origins, not '*'",
        );
    }
    for (i, origin) in cors.allowed_origins.iter().enumerate() {
        c.require(
            origin == "*" || is_valid_origin(origin),
            &format!("{prefix}.allowed_origins[{i}]"),
            "must be '*' or an absolute origin (scheme://host[:port])",
        );
    }
    const KNOWN: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    for (i, m) in cors.allowed_methods.iter().enumerate() {
        c.require(
            KNOWN.contains(&m.as_str()),
            &format!("{prefix}.allowed_methods[{i}]"),
            "unknown HTTP method",
        );
    }
}

fn is_valid_origin(origin: &str) -> bool {
    url::Url::parse(origin)
        .map(|u| u.origin().is_tuple() && u.path() == "/" && u.query().is_none())
        .unwrap_or(false)
}

#[cfg(test)]
#[allow(clippy::too_many_lines)]
mod tests {
    use super::*;
    use serde_json::json;

    fn http_upstream() -> Upstream {
        serde_json::from_value(json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "api.example.com", "port": 80 } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }))
        .unwrap()
    }

    #[test]
    fn scheme_accepts_http_and_the_tls_family() {
        for (raw, expected) in [
            ("http", Scheme::Http),
            ("https", Scheme::Https),
            ("wss", Scheme::Wss),
            ("wt", Scheme::Wt),
            ("grpc", Scheme::Grpc),
        ] {
            let ep: Endpoint =
                serde_json::from_value(json!({ "scheme": raw, "host": "h" })).unwrap();
            assert_eq!(ep.scheme, expected, "scheme {raw}");
        }
        // Omitted scheme defaults to https.
        let ep: Endpoint = serde_json::from_value(json!({ "host": "h" })).unwrap();
        assert_eq!(ep.scheme, Scheme::Https);
    }

    #[test]
    fn unknown_scheme_is_rejected() {
        let err = serde_json::from_value::<Upstream>(json!({
            "server": { "endpoints": [ { "scheme": "ftp", "host": "h" } ] },
            "protocol": "http"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown variant") || err.to_string().contains("ftp"));
    }

    #[test]
    fn protocol_accepts_gts_and_bare_forms() {
        let u: Upstream = serde_json::from_value(json!({
            "server": { "endpoints": [ { "host": "h" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }))
        .unwrap();
        assert_eq!(u.protocol, Protocol::Http);
        let u: Upstream = serde_json::from_value(json!({
            "server": { "endpoints": [ { "host": "h" } ] }, "protocol": "grpc"
        }))
        .unwrap();
        assert_eq!(u.protocol, Protocol::Grpc);
        // Serializes back to the canonical GTS identifier.
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["protocol"], crate::gts::PROTOCOL_GRPC);
    }

    #[test]
    fn endpoints_default_to_the_scheme_port() {
        let ep: Endpoint =
            serde_json::from_value(json!({ "scheme": "http", "host": "h" })).unwrap();
        assert_eq!(ep.effective_port(), 80);
        assert_eq!(ep.host_header(), "h");
        let ep: Endpoint =
            serde_json::from_value(json!({ "scheme": "https", "host": "h", "port": 8443 }))
                .unwrap();
        assert_eq!(ep.effective_port(), 8443);
        assert_eq!(ep.host_header(), "h:8443");
    }

    #[test]
    fn host_normalization_strips_dots_and_brackets() {
        let ep: Endpoint = serde_json::from_value(json!({ "host": "Example.COM." })).unwrap();
        assert_eq!(ep.normalized_host(), "example.com");
        let ep: Endpoint =
            serde_json::from_value(json!({ "host": "[::1]", "port": 8080 })).unwrap();
        assert_eq!(ep.normalized_host(), "::1");
        assert_eq!(ep.host_header(), "[::1]:8080");
    }

    #[test]
    fn upstream_requires_endpoints() {
        // `server` and `protocol` are required on the wire; the empty
        // `endpoints` array is what validation reports.
        let raw = serde_json::from_value::<Upstream>(json!({
            "protocol": "http",
            "server": { "endpoints": [] }
        }))
        .unwrap();
        let err = validate_upstream(&raw).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field == "server.endpoints"))
        );
    }

    #[test]
    fn upstream_happy_path_is_valid() {
        let mut u = http_upstream();
        u.alias = Some("api.example.com".to_owned());
        u.tags = vec!["openai".to_owned(), "llm".to_owned()];
        u.auth = Some(AuthConfig {
            plugin_type: Some(crate::gts::AUTH_APIKEY.to_owned()),
            sharing: Some(SharingMode::Private),
            config: Some(json!({ "api_key_ref": "openai-key" })),
        });
        assert!(validate_upstream(&u).is_ok(), "{u:?}");
    }

    #[test]
    fn upstream_rejects_empty_endpoints() {
        let mut u = http_upstream();
        u.server.endpoints.clear();
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field == "server.endpoints"))
        );
    }

    #[test]
    fn upstream_rejects_bad_host() {
        let mut u = http_upstream();
        u.server.endpoints[0].host = "not a host".to_owned();
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains(".host")))
        );
    }

    #[test]
    fn ssrf_policy_denies_loopback_host() {
        let mut u = http_upstream();
        u.server.endpoints[0].host = "127.0.0.1".to_owned();
        let err = validate_upstream_ssrf(&u, &crate::config::SsrfPolicy::default()).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains(".host")))
        );

        // The shape validator does not care: which values the field accepts is
        // a schema question, not a policy one.
        assert!(validate_upstream(&u).is_ok(), "{u:?}");

        // A disabled policy admits the endpoint.
        let off = crate::config::SsrfPolicy {
            enabled: false,
            ..Default::default()
        };
        assert!(validate_upstream_ssrf(&u, &off).is_ok());
    }

    #[test]
    fn upstream_rejects_duplicate_endpoints() {
        let mut u = http_upstream();
        u.server.endpoints.push(u.server.endpoints[0].clone());
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.message.contains("duplicate")))
        );
    }

    #[test]
    fn upstream_rejects_invalid_alias_characters() {
        let mut u = http_upstream();
        u.alias = Some("Bad Alias!".to_owned());
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field == "alias"))
        );
    }

    #[test]
    fn upstream_rejects_auth_without_type() {
        let mut u = http_upstream();
        u.auth = Some(AuthConfig::default());
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field == "auth.type"))
        );
    }

    #[test]
    fn upstream_rejects_tags_with_bad_characters() {
        let mut u = http_upstream();
        u.tags = vec!["Not A Tag".to_owned()];
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains("tags")))
        );
    }

    #[test]
    fn upstream_rejects_rate_limit_below_one() {
        let mut u = http_upstream();
        u.rate_limit = Some(serde_json::from_value(json!({ "sustained": { "rate": 0 } })).unwrap());
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains("sustained.rate")))
        );
    }

    #[test]
    fn upstream_rejects_cors_wildcard_with_credentials() {
        let mut u = http_upstream();
        u.cors = Some(
            serde_json::from_value(json!({
                "enabled": true,
                "allow_credentials": true,
                "allowed_origins": ["*"]
            }))
            .unwrap(),
        );
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains("allowed_origins")))
        );
    }

    #[test]
    fn upstream_rejects_bad_cors_origin() {
        let mut u = http_upstream();
        u.cors = Some(
            serde_json::from_value(json!({
                "enabled": true,
                "allowed_origins": ["not-an-origin"]
            }))
            .unwrap(),
        );
        let err = validate_upstream(&u).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains("allowed_origins[0]")))
        );
    }

    /// A structurally valid HTTP route, for fixtures that tweak one field.
    fn valid_route() -> Route {
        serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/a" } }
        }))
        .unwrap()
    }

    #[test]
    fn route_requires_exactly_one_match() {
        // `match` is required on the wire, so the fixture with neither match
        // arm is built structurally rather than through serde.
        let mut none = valid_route();
        none.r#match = RouteMatch::default();
        assert!(validate_route(&none).is_err());

        let both: Route = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": {
                "http": { "methods": ["GET"], "path": "/a" },
                "grpc": { "service": "s", "method": "m" }
            }
        }))
        .unwrap();
        assert!(validate_route(&both).is_err());

        let http: Route = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/a" } }
        }))
        .unwrap();
        assert!(validate_route(&http).is_ok(), "{http:?}");

        let grpc: Route = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "grpc": { "service": "s", "method": "m" } }
        }))
        .unwrap();
        assert!(validate_route(&grpc).is_ok(), "{grpc:?}");
    }

    #[test]
    fn route_requires_methods_and_path() {
        let no_methods: Route = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": [], "path": "/a" } }
        }))
        .unwrap();
        let err = validate_route(&no_methods).unwrap_err();
        assert!(
            matches!(err, DomainError::Validation(ref i) if i.iter().any(|x| x.field.contains("methods")))
        );

        let empty_path: Route = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "" } }
        }))
        .unwrap();
        assert!(validate_route(&empty_path).is_err());
    }

    #[test]
    fn route_defaults_are_applied() {
        let r: Route = serde_json::from_value(json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["POST"], "path": "/v1/x" } }
        }))
        .unwrap();
        assert!(r.enabled);
        assert_eq!(r.priority, 0);
        assert_eq!(
            r.r#match.http.as_ref().unwrap().path_suffix_mode,
            PathSuffixMode::Append
        );
        assert!(r.plugins.items.is_empty());
    }

    #[test]
    fn plugin_items_accept_string_and_inline_forms() {
        let u: Upstream = serde_json::from_value(json!({
            "server": { "endpoints": [ { "host": "h" } ] },
            "protocol": "http",
            "plugins": {
                "items": [
                    crate::gts::TRANSFORM_REQUEST_ID,
                    { "type": "cf.core.oagw.required_headers.v1", "config": { "required_request_headers": "x-a" } }
                ]
            }
        }))
        .unwrap();
        assert_eq!(u.plugins.items.len(), 2);
        assert!(matches!(&u.plugins.items[0], PluginItem::Reference(_)));
        assert!(matches!(&u.plugins.items[1], PluginItem::Inline(_)));
        assert!(validate_upstream(&u).is_ok(), "{u:?}");
    }

    #[test]
    fn plugin_requires_source() {
        let p = Plugin {
            id: None,
            kind: PluginKind::Transform,
            name: Some("x".to_owned()),
            description: None,
            tags: vec![],
            config: None,
            source: None,
        };
        assert!(validate_plugin(&p).is_err());
        let p = Plugin {
            source: Some("def transform(ctx): pass".to_owned()),
            ..p
        };
        assert!(validate_plugin(&p).is_ok());
    }

    #[test]
    fn plugin_kind_parses_names_and_gts_bases() {
        assert_eq!(PluginKind::parse("auth").unwrap(), PluginKind::Auth);
        assert_eq!(
            PluginKind::parse(crate::gts::GUARD_PLUGIN_TYPE).unwrap(),
            PluginKind::Guard
        );
        assert_eq!(
            PluginKind::parse(crate::gts::TRANSFORM_PLUGIN_TYPE).unwrap(),
            PluginKind::Transform
        );
        assert!(PluginKind::parse("nope").is_none());
    }

    #[test]
    fn sharing_mode_defaults_to_private() {
        let pc: PluginsConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(pc.sharing, SharingMode::Private);
    }

    #[test]
    fn rate_limit_burst_defaults_to_sustained_rate() {
        let rl: RateLimitConfig =
            serde_json::from_value(json!({ "sustained": { "rate": 10, "window": "minute" } }))
                .unwrap();
        assert_eq!(rl.capacity(), 10.0);
        assert!((rl.rate_per_second() - 10.0 / 60.0).abs() < 1e-9);
        let rl: RateLimitConfig = serde_json::from_value(json!({
            "sustained": { "rate": 10 }, "burst": { "capacity": 50 }
        }))
        .unwrap();
        assert_eq!(rl.capacity(), 50.0);
    }

    #[test]
    fn cors_methods_default_to_get_and_post() {
        let c: CorsConfig = serde_json::from_value(json!({ "enabled": true })).unwrap();
        assert_eq!(c.methods(), &["GET".to_owned(), "POST".to_owned()]);
        assert!(!c.credentials());
    }

    #[test]
    fn http_method_parses_request_methods() {
        assert_eq!(HttpMethod::parse(&http::Method::GET), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::parse(&http::Method::HEAD), None);
        assert_eq!(HttpMethod::parse(&http::Method::TRACE), None);
    }
}
