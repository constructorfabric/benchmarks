//! Domain entities for the OAGW control plane.
//!
//! These types are also the wire shapes: the JSON field names follow
//! `docs/schemas/upstream.v1.schema.json` and `route.v1.schema.json`. The
//! `http` endpoint scheme is *additionally* accepted beyond the documented
//! enum because the deployment contract declares `allow_http_upstream: true`.

use std::collections::BTreeMap;

use uuid::Uuid;

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// Serde default for the `enabled` switch (PRD `cpt-cf-oagw-fr-enable-disable`):
/// a newly created upstream or route accepts traffic unless the caller
/// explicitly disables it.
#[must_use]
pub fn default_true() -> bool {
    true
}

/// Endpoint transport scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP/1.1. Only legal when `allow_http_upstream` is `true`.
    Http,
    /// TLS HTTP/1.1 or HTTP/2.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebSocket-Transport.
    Wt,
    /// HTTP/2-based gRPC.
    Grpc,
}

impl EndpointScheme {
    /// Port implied by the scheme when `endpoint.port` is omitted.
    #[must_use]
    pub fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// `true` when the connection must be TLS-wrapped before any HTTP bytes.
    #[must_use]
    pub fn needs_tls(self) -> bool {
        !matches!(self, Self::Http)
    }
}

/// Upstream protocol selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    /// HTTP (default).
    #[default]
    Http,
    /// gRPC (planned / phase 3 — matched but not proxied).
    Grpc,
}

impl serde::Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.gts_id())
    }
}

impl<'de> serde::Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let lowered = raw.to_ascii_lowercase();
        if lowered.ends_with("grpc.v1") {
            Ok(Self::Grpc)
        } else {
            Ok(Self::Http)
        }
    }
}

impl Protocol {
    /// The fully-qualified GTS id written on the wire.
    #[must_use]
    pub fn gts_id(self) -> &'static str {
        match self {
            Self::Http => crate::domain::gts_helpers::PROTOCOL_HTTP,
            Self::Grpc => crate::domain::gts_helpers::PROTOCOL_GRPC,
        }
    }
}

/// Sharing mode for hierarchical configuration inheritance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Endpoint {
    /// Transport scheme.
    pub scheme: EndpointScheme,
    /// Hostname or IP literal.
    pub host: String,
    /// Resolved port; `None` means the scheme's standard port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            scheme: EndpointScheme::Https,
            host: String::new(),
            port: None,
        }
    }
}

impl Endpoint {
    /// Effective port (explicit value or the scheme's standard port).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.standard_port())
    }

    /// `host` normalised for alias derivation: lowercased, trailing dot and
    /// surrounding whitespace removed.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        self.host
            .trim()
            .trim_end_matches('.')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase()
    }
}

/// Endpoint list, as the wire `server` object.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Server {
    /// One or more upstream addresses.
    pub endpoints: Vec<Endpoint>,
}

/// Which inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward nothing beyond what the gateway itself must set.
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything except hop-by-hop and routing headers.
    All,
}

/// Inbound/outbound header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HeaderRules {
    /// Set (overwrite) these headers.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append these headers (may duplicate).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Remove these inbound headers.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded upstream.
    pub passthrough: Passthrough,
    /// Headers forwarded when [`HeaderRules::passthrough`] is
    /// [`Passthrough::Allowlist`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Request/response header transformation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HeadersConfig {
    /// Applied on the way out to the upstream.
    pub request: HeaderRules,
    /// Applied on the way back to the client.
    pub response: HeaderRules,
}

impl HeadersConfig {
    /// `true` when no rule is configured, so the field is elided on the wire.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request == HeaderRules::default() && self.response == HeaderRules::default()
    }
}

/// Sustained-rate component of a dual-rate limit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SustainedRate {
    /// Tokens replenished per [`SustainedRate::window`].
    pub rate: u32,
    /// Window length.
    pub window: RateWindow,
}

impl Default for SustainedRate {
    fn default() -> Self {
        Self {
            rate: 1,
            window: RateWindow::Second,
        }
    }
}

/// Time window of a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
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
    /// Length of the window in seconds.
    #[must_use]
    pub fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Burst component of a dual-rate limit (token-bucket capacity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Burst {
    /// Maximum burst size.
    pub capacity: u32,
}

impl Default for Burst {
    fn default() -> Self {
        Self { capacity: 1 }
    }
}

/// Dual-rate limit configuration (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Rate-limiting algorithm.
    pub algorithm: RateAlgorithm,
    /// Sustained rate (refill).
    pub sustained: SustainedRate,
    /// Burst capacity (bucket size).
    pub burst: Burst,
    /// Counter scope.
    pub scope: RateScope,
    /// Behaviour when the limit is exceeded.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate::default(),
            burst: Burst::default(),
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Classic token bucket (bursts allowed).
    #[default]
    TokenBucket,
    /// Sliding window (no boundary bursts). Treated as a token bucket with
    /// capacity equal to the sustained rate.
    SlidingWindow,
}

/// Counter scope for a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
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

/// Behaviour when a rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Queue (not implemented — falls back to [`RateStrategy::Reject`]).
    Queue,
    /// Degrade (not implemented — falls back to [`RateStrategy::Reject`]).
    Degrade,
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Master switch.
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any.
    pub allowed_origins: Vec<String>,
    /// Allowed methods (preflight and actual cross-origin requests).
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials (cookies / auth headers).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec![String::from("GET"), String::from("POST")],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Reference to a plugin: a built-in GTS id or a custom plugin instance id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginRef {
    /// Built-in named plugin (`gts.cf.core.oagw.<family>.v1~cf.core.oagw.<name>.v1`).
    Named(String),
    /// Custom plugin referenced by its UUID tail.
    Custom(Uuid),
}

impl std::fmt::Display for PluginRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Named(id) => write!(f, "{id}"),
            Self::Custom(uuid) => write!(f, "{uuid}"),
        }
    }
}

/// Parse a wire plugin reference into a built-in name or a custom UUID.
#[must_use]
pub fn parse_plugin_ref(raw: &str) -> PluginRef {
    let tail = raw.split('~').next_back().unwrap_or(raw);
    match Uuid::parse_str(tail) {
        Ok(uuid) => PluginRef::Custom(uuid),
        Err(_) => PluginRef::Named(raw.to_owned()),
    }
}

/// Extract the short built-in plugin name from a GTS id, if it is one.
///
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` → `Some("apikey")`.
/// The bare instance id (`cf.core.oagw.apikey.v1`) and a plain short name
/// (`apikey`) are accepted as well, so a configuration that omits part of the
/// family still resolves to the same plugin. A UUID tail means a tenant-defined
/// plugin and yields `None`.
#[must_use]
pub fn builtin_plugin_name(raw: &str) -> Option<String> {
    let tail = raw.split('~').next_back()?;
    if Uuid::parse_str(tail).is_ok() {
        return None;
    }
    let short = tail.strip_suffix(".v1").unwrap_or(tail);
    Some(short.strip_prefix("cf.core.oagw.").unwrap_or(short).to_owned())
}

/// Auth plugin ids that are *catalogued* but have no implementation in this
/// build (DESIGN §Plugin catalog).
pub const CATALOG_ONLY_AUTH_PLUGINS: [&str; 2] = ["basic", "bearer"];

/// Guard plugin ids that are catalogued but unimplemented.
pub const CATALOG_ONLY_GUARD_PLUGINS: [&str; 2] = ["timeout", "cors"];

/// Transform plugin ids that are catalogued but unimplemented.
pub const CATALOG_ONLY_TRANSFORM_PLUGINS: [&str; 2] = ["logging", "metrics"];

/// Built-in plugin ids that are *catalogued* but have no implementation in this
/// build (DESIGN §Plugin catalog): auth `basic` / `bearer`, guard `timeout` /
/// `cors`, transform `logging` / `metrics`. They are advertised by
/// `GET /plugins/catalog` and rejected at binding time.
pub const CATALOG_ONLY_PLUGINS: [&str; 6] = [
    "basic", "bearer", "timeout", "cors", "logging", "metrics",
];

/// `true` when `name` is a reserved, catalog-only plugin id.
///
/// `basic` / `bearer` are auth-only, `timeout` / `cors` guard-only and
/// `logging` / `metrics` transform-only, but keeping the check family-blind is
/// deliberate: a reserved id never resolves, whichever family binds it.
#[must_use]
pub fn is_catalog_only_plugin(name: &str) -> bool {
    // Bindings are written as GTS ids
    // (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1`), so compare
    // the short name, falling back to the raw string for anything that is not a
    // documented plugin id (a custom plugin's UUID, say).
    let short = builtin_plugin_name(name).unwrap_or_else(|| name.to_owned());
    CATALOG_ONLY_PLUGINS.contains(&short.as_str())
}

/// Plugin chain binding.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PluginBindings {
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Ordered plugin references.
    pub items: Vec<String>,
}

impl PluginBindings {
    /// `true` when no plugin is bound, so the field is elided on the wire.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Auth plugin configuration (`auth` object on an upstream).
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Auth plugin GTS id (`type` on the wire, `plugin_type` accepted too).
    #[serde(rename = "type", alias = "plugin_type")]
    pub plugin_type: Option<String>,
    /// Plugin instance id (custom plugins only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_ref: Option<String>,
    /// Hierarchical sharing mode.
    pub sharing: Sharing,
    /// Opaque plugin configuration, interpreted by the selected plugin.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// Writable subset of an `Upstream`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UpstreamConfig {
    /// Whether this upstream accepts traffic. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit routing alias. `None` → derived from the endpoints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat categorisation tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Upstream endpoints.
    pub server: Server,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Outbound authentication plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginBindings::is_empty")]
    pub plugins: PluginBindings,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: Server::default(),
            protocol: Protocol::default(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginBindings::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

impl UpstreamConfig {
    /// Normalise an explicit alias: trim, drop the trailing dot, lowercase.
    #[must_use]
    pub fn normalize_alias(raw: &str) -> String {
        raw.trim().trim_end_matches('.').to_ascii_lowercase()
    }

    /// Validate and return the alias, explicit or derived.
    ///
    /// # Errors
    /// Returns a validation message when neither the explicit alias nor the
    /// endpoint set can produce one, or when the explicit alias is not
    /// syntactically valid.
    pub fn resolve_alias(&self) -> Result<String, String> {
        if let Some(alias) = self
            .alias
            .as_deref()
            .map(Self::normalize_alias)
            .filter(|a| !a.is_empty())
        {
            if !is_valid_alias(&alias) {
                return Err(format!(
                    "alias '{alias}' does not match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
                ));
            }
            return Ok(alias);
        }
        derive_alias(&self.server.endpoints).ok_or_else(|| {
            String::from(
                "alias is required: IP-based or non-derivable endpoints need an explicit alias",
            )
        })
    }
}

/// Persisted upstream.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Creation instant (epoch seconds).
    pub created_at: u64,
    /// Writable configuration (flattened onto the resource object).
    #[serde(flatten)]
    pub config: UpstreamConfig,
}

impl Upstream {
    /// The resolved routing alias.
    #[must_use]
    pub fn alias(&self) -> Option<&str> {
        self.config.alias.as_deref()
    }
}

/// HTTP match rules for a route.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HttpMatch {
    /// Allowed methods.
    pub methods: Vec<String>,
    /// Path pattern (prefix).
    pub path: String,
    /// Allowed query parameters; empty means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` is treated.
    pub path_suffix_mode: PathSuffixMode,
}

/// How the proxy path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a suffix.
    Disabled,
    /// Append the suffix to [`HttpMatch::path`].
    #[default]
    Append,
}

/// gRPC match rules (phase 3).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct GrpcMatch {
    /// Fully-qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchRules {
    /// HTTP match.
    Http(HttpMatch),
    /// gRPC match.
    Grpc(GrpcMatch),
}

impl Default for MatchRules {
    fn default() -> Self {
        Self::Http(HttpMatch::default())
    }
}

/// Writable subset of a `Route`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RouteConfig {
    /// Owning upstream (immutable after create).
    pub upstream_id: Uuid,
    /// Whether this route participates in matching. Defaults to `true`; a
    /// disabled route is excluded from route matching (PRD
    /// `cpt-cf-oagw-fr-enable-disable`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Flat categorisation tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match")]
    pub matcher: MatchRules,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginBindings::is_empty")]
    pub plugins: PluginBindings,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl Default for RouteConfig {
    fn default() -> Self {
        Self {
            upstream_id: Uuid::nil(),
            enabled: true,
            tags: Vec::new(),
            matcher: MatchRules::default(),
            plugins: PluginBindings::default(),
            rate_limit: None,
        }
    }
}

/// Persisted route.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream (denormalised for lookup).
    pub upstream_id: Uuid,
    /// Creation instant (epoch seconds).
    pub created_at: u64,
    /// Writable configuration.
    #[serde(flatten)]
    pub config: RouteConfig,
}

/// A tenant-defined (custom) plugin.
#[derive(Debug, Clone, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Plugin {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Creation instant (epoch seconds).
    pub created_at: u64,
    /// Auth / guard / transform.
    pub plugin_type: PluginKind,
    /// Human-readable name.
    pub name: String,
    /// Starlark (or otherwise sandboxed) source text.
    pub source: String,
    /// Per-binding configuration the plugin is executed with.
    ///
    /// A custom plugin row is the only place a binding can carry
    /// configuration (`oagw_plugin` is the row a UUID reference resolves to),
    /// so a custom plugin that wraps a built-in implementation passes its
    /// configuration to that built-in.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

/// Which plugin family a custom plugin belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Credential injection.
    #[default]
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request/response mutation.
    Transform,
}

// ---------------------------------------------------------------------------
// Alias derivation
// ---------------------------------------------------------------------------

/// Standard port used by `http` endpoints.
const HTTP_STANDARD_PORT: u16 = 80;
/// Standard port used by every TLS-carrying endpoint scheme.
const TLS_STANDARD_PORT: u16 = 443;

/// Alias characters accepted by `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.len() < 2 {
        return false;
    }
    let first_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let last = bytes[bytes.len() - 1];
    let last_ok = last.is_ascii_lowercase() || last.is_ascii_digit();
    let middle_ok = bytes[1..bytes.len() - 1]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-'));
    first_ok && last_ok && middle_ok
}

/// Derive the routing alias from an endpoint set (DESIGN §3.3 alias table).
///
/// Returns `None` when no alias can be derived — IP literals, bare public
/// suffixes, host lists without a common registrable suffix, or mixed
/// schemes/ports.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    let first_scheme = first.scheme;
    let first_port = first.port();

    for endpoint in endpoints {
        if endpoint.scheme != first_scheme || endpoint.port() != first_port {
            return None;
        }
        if endpoint.normalized_host().is_empty() {
            return None;
        }
    }

    let hosts: Vec<String> = endpoints.iter().map(Endpoint::normalized_host).collect();

    // Any IP literal → not derivable.
    if hosts.iter().any(|h| is_ip_literal(h)) {
        return None;
    }

    let port_suffix = if first_port == HTTP_STANDARD_PORT || first_port == TLS_STANDARD_PORT {
        String::new()
    } else {
        format!(":{first_port}")
    };

    if hosts.len() == 1 {
        let candidate = format!("{}{port_suffix}", hosts[0]);
        // A bare public suffix ("co.uk") is not a usable alias; a single-label
        // host ("localhost") is.
        if !is_valid_alias(&candidate) || is_bare_public_suffix(&hosts[0]) {
            return None;
        }
        return Some(candidate);
    }

    // Multiple hostnames: the longest common label suffix must be a
    // registrable domain (PSL-wise) and at least two labels long.
    let first_labels: Vec<&str> = hosts[0].split('.').collect();
    let mut suffix_len = first_labels.len();
    for host in &hosts[1..] {
        let labels: Vec<&str> = host.split('.').collect();
        let min = suffix_len.min(labels.len());
        let mut common = 0usize;
        while common < min
            && first_labels[first_labels.len() - 1 - common]
                .eq_ignore_ascii_case(labels[labels.len() - 1 - common])
        {
            common += 1;
        }
        suffix_len = suffix_len.min(common);
        if suffix_len == 0 {
            return None;
        }
    }
    if suffix_len < 2 {
        return None;
    }
    let suffix = first_labels[first_labels.len() - suffix_len..].join(".");
    let candidate = format!("{suffix}{port_suffix}");
    if !is_valid_alias(&candidate) || psl::domain_str(&suffix).is_none() {
        return None;
    }
    Some(candidate)
}

/// `true` when `host` is itself a public suffix (`co.uk`) rather than a
/// registrable name. Single-label hosts (`localhost`) never count.
#[must_use]
pub fn is_bare_public_suffix(host: &str) -> bool {
    host.contains('.') && psl::suffix_str(host) == Some(host)
}

/// `true` for IPv4 / IPv6 literals (including bracketed IPv6).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    if host.parse::<std::net::Ipv4Addr>().is_ok() || host.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    host.contains(':')
}

/// `true` for a syntactically valid RFC 1123 hostname.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_with_standard_port_derives_bare_host() {
        let eps = vec![ep(EndpointScheme::Https, "api.openai.com", None)];
        assert_eq!(derive_alias(&eps).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_with_nonstandard_port_derives_host_port() {
        let eps = vec![ep(EndpointScheme::Https, "api.openai.com", Some(8443))];
        assert_eq!(derive_alias(&eps).as_deref(), Some("api.openai.com:8443"));
    }

    #[test]
    fn http_uses_port_80_as_standard() {
        let eps = vec![ep(EndpointScheme::Http, "localhost", Some(80))];
        assert_eq!(derive_alias(&eps).as_deref(), Some("localhost"));
        let eps = vec![ep(EndpointScheme::Http, "localhost", Some(8080))];
        assert_eq!(derive_alias(&eps).as_deref(), Some("localhost:8080"));
    }

    #[test]
    fn multi_host_common_suffix_is_registrable() {
        let eps = vec![
            ep(EndpointScheme::Https, "us.vendor.com", None),
            ep(EndpointScheme::Https, "eu.vendor.com", None),
        ];
        assert_eq!(derive_alias(&eps).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_host_non_standard_port_keeps_the_port() {
        let eps = vec![
            ep(EndpointScheme::Https, "us.vendor.com", Some(8443)),
            ep(EndpointScheme::Https, "eu.vendor.com", Some(8443)),
        ];
        assert_eq!(derive_alias(&eps).as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn mixed_ports_are_not_derivable() {
        let eps = vec![
            ep(EndpointScheme::Https, "us.vendor.com", Some(443)),
            ep(EndpointScheme::Https, "eu.vendor.com", Some(8443)),
        ];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let eps = vec![ep(EndpointScheme::Https, "co.uk", None)];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn ip_literals_require_an_explicit_alias() {
        let eps = vec![ep(EndpointScheme::Http, "127.0.0.1", Some(8080))];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn alias_validation_rejects_bad_shapes() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("my-service"));
        assert!(is_valid_alias("host:8443"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("a"));
        assert!(!is_valid_alias("-leading"));
        assert!(!is_valid_alias("UPPER"));
        assert!(!is_valid_alias("trail."));
    }

    #[test]
    fn hostnames_are_validated() {
        assert!(is_valid_hostname("api.openai.com"));
        assert!(is_valid_hostname("a"));
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname("a..b"));
        assert!(!is_valid_hostname("-lead"));
    }

    #[test]
    fn protocol_round_trips_through_the_gts_id() {
        let parsed: Protocol =
            serde_json::from_value(serde_json::json!(crate::domain::gts_helpers::PROTOCOL_HTTP))
                .expect("http protocol");
        assert_eq!(parsed, Protocol::Http);
        assert_eq!(
            serde_json::to_value(Protocol::Grpc).unwrap(),
            serde_json::json!(crate::domain::gts_helpers::PROTOCOL_GRPC)
        );
    }

    #[test]
    fn built_in_plugin_names_are_extracted() {
        assert_eq!(
            builtin_plugin_name("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
                .as_deref(),
            Some("apikey")
        );
        assert_eq!(
            builtin_plugin_name("550e8400-e29b-41d4-a716-446655440000"),
            None
        );
    }

    #[test]
    fn plugin_refs_split_named_from_custom() {
        assert_eq!(
            parse_plugin_ref("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
            PluginRef::Named("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned())
        );
        assert_eq!(
            parse_plugin_ref("00000000-0000-0000-0000-000000000000"),
            PluginRef::Custom(Uuid::nil())
        );
        assert!(matches!(
            parse_plugin_ref("550e8400-e29b-41d4-a716-446655440000"),
            PluginRef::Custom(_)
        ));
    }

    // ── `enabled` defaults ────────────────────────────────────────────────

    /// D1: an upstream created without an `enabled` field accepts traffic.
    #[test]
    fn upstream_enabled_defaults_to_true() {
        let config: UpstreamConfig =
            serde_json::from_str(r#"{"server":{"endpoints":[{"scheme":"http","host":"127.0.0.1","port":9399}]}}"#)
                .expect("upstream config");
        assert!(config.enabled, "absent `enabled` must mean enabled");
        let wire = serde_json::to_value(&config).expect("serialize");
        assert_eq!(wire["enabled"], serde_json::Value::Bool(true));

        // An explicit `false` is honoured (the disable path still works).
        let config: UpstreamConfig =
            serde_json::from_str(r#"{"enabled":false,"server":{"endpoints":[{"scheme":"http","host":"127.0.0.1","port":9399}]}}"#)
                .expect("upstream config");
        assert!(!config.enabled);
    }

    /// D6: a route without an `enabled` field participates in matching, and an
    /// explicit `false` removes it.
    #[test]
    fn route_enabled_defaults_to_true() {
        let config: RouteConfig = serde_json::from_str(
            r#"{"upstream_id":"00000000-0000-0000-0000-000000000000","match":{"http":{"methods":["GET"],"path":"/x"}}}"#,
        )
        .expect("route config");
        assert!(config.enabled, "absent `enabled` must mean enabled");
        assert_eq!(
            serde_json::to_value(&config).expect("serialize")["enabled"],
            serde_json::Value::Bool(true)
        );

        let config: RouteConfig = serde_json::from_str(
            r#"{"upstream_id":"00000000-0000-0000-0000-000000000000","enabled":false,"match":{"http":{"methods":["GET"],"path":"/x"}}}"#,
        )
        .expect("route config");
        assert!(!config.enabled);
    }

    /// D7: the reserved, catalog-only ids are recognised in both the short and
    /// the fully-qualified spelling, and never for a custom plugin id.
    #[test]
    fn catalog_only_plugins_are_recognised_by_id_and_short_name() {
        for reserved in ["basic", "bearer", "timeout", "cors", "logging", "metrics"] {
            assert!(is_catalog_only_plugin(reserved), "{reserved}");
        }
        for reserved in [
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
        ] {
            assert!(is_catalog_only_plugin(reserved), "{reserved}");
        }
        for implemented in [
            "noop",
            "apikey",
            "oauth2_client_cred",
            "oauth2_client_cred_basic",
            "required_headers",
            "request_id",
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
        ] {
            assert!(!is_catalog_only_plugin(implemented), "{implemented}");
        }
        assert!(
            !is_catalog_only_plugin("550e8400-e29b-41d4-a716-446655440000"),
            "a custom plugin id is never reserved"
        );
    }
}
