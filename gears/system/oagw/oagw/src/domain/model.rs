//! OAGW domain model: upstreams, routes, plugins, and their configuration
//! payloads.
//!
//! The wire shape mirrors `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`; the deserialization is deliberately
//! permissive (no `deny_unknown_fields`) so callers can round-trip payloads
//! that carry schema-extension members.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// GTS identifier of the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS identifier of the gRPC upstream protocol (catalogued; no proxy path).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Base type of the upstream resource.
pub const UPSTREAM_GTS_BASE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Base type of the route resource.
pub const ROUTE_GTS_BASE: &str = "gts.cf.core.oagw.route.v1~";
/// Base type of the custom plugin resource.
pub const PLUGIN_GTS_BASE: &str = "gts.cf.core.oagw.plugin.v1~";

/// Built-in auth plugin GTS identifiers.
pub mod auth_plugin_ids {
    /// No-op authentication.
    pub const NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// API key injection.
    pub const APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// `OAuth2` client credentials, credentials in the form body.
    pub const OAUTH2_CC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// `OAuth2` client credentials, credentials in `Authorization: Basic`.
    pub const OAUTH2_CC_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// Catalogued-only Basic auth (no backing implementation).
    pub const BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    /// Catalogued-only Bearer injection.
    pub const BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
}

/// Built-in guard plugin GTS identifiers.
pub mod guard_plugin_ids {
    /// Required header enforcement.
    pub const REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Catalogued-only: core data-plane timeout logic.
    pub const TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
    /// Catalogued-only: core data-plane CORS logic.
    pub const CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
}

/// Built-in transform plugin GTS identifiers.
pub mod transform_plugin_ids {
    /// `X-Request-ID` propagation.
    pub const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
    /// Catalogued-only: core data-plane logging.
    pub const LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
    /// Catalogued-only: core data-plane metrics.
    pub const METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";
}

/// How a configuration field is shared down the tenant hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may read and override.
    Inherit,
    /// Descendants must obey this value.
    Enforce,
}

/// Upstream endpoint scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum EndpointScheme {
    /// Plaintext HTTP. Accepted at parse time; dialing is gated by
    /// `allow_http_upstream`.
    Http,
    /// HTTPS.
    #[default]
    Https,
    /// Plaintext WebSocket.
    Ws,
    /// Secure WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
    /// Secure gRPC.
    Grpcs,
}

impl EndpointScheme {
    /// The scheme actually dialled on the wire.
    ///
    /// `ws` speaks HTTP/1.1 with an upgrade and `grpc` speaks HTTP/2, both in
    /// cleartext unless their secure spelling was configured, so the three
    /// plaintext schemes share `http`'s dial scheme — and with it the
    /// `allow_http_upstream` gate.
    #[must_use]
    pub fn dial_scheme(self) -> &'static str {
        match self {
            Self::Http | Self::Ws | Self::Grpc => "http",
            Self::Https | Self::Wss | Self::Wt | Self::Grpcs => "https",
        }
    }

    /// Whether the endpoint is dialled in cleartext, i.e. subject to the
    /// `allow_http_upstream` gate.
    #[must_use]
    pub fn is_cleartext(self) -> bool {
        self.dial_scheme() == "http"
    }

    /// Whether the endpoint is expected to speak TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !self.is_cleartext()
    }

    /// Whether the scheme carries a WebSocket-style upgrade.
    #[must_use]
    pub fn is_websocket(self) -> bool {
        matches!(self, Self::Ws | Self::Wss)
    }

    /// Port assumed when the endpoint omits one.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc | Self::Grpcs => 443,
        }
    }
}

/// One upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Endpoint {
    /// URI scheme.
    pub scheme: EndpointScheme,
    /// Hostname or IP literal.
    pub host: String,
    /// Port; `None` means the scheme's default.
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port (explicit or scheme default).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// `true` when the port is the scheme's default.
    #[must_use]
    pub fn uses_standard_port(&self) -> bool {
        self.effective_port() == self.scheme.default_port()
    }

    /// `host` with an IPv6 literal bracketed, as an authority requires.
    ///
    /// An unbracketed `::1` reads as `host ""`, port `1` (`::1:8443` would not
    /// parse at all), so an IPv6 literal always travels in brackets.
    #[must_use]
    pub fn bracketed_host(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// `host` or `host:port` for a non-standard port, as an authority: the
    /// `Host` header value and the authority part of the upstream URL.
    #[must_use]
    pub fn authority(&self) -> String {
        let host = self.bracketed_host();
        if self.uses_standard_port() {
            host
        } else {
            format!("{host}:{}", self.effective_port())
        }
    }

    /// Plaintext endpoint (dialing governed by `allow_http_upstream`).
    #[must_use]
    pub fn is_plaintext(&self) -> bool {
        !self.scheme.is_tls()
    }
}

/// `server` block of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ServerConfig {
    /// Endpoint pool. All entries must share protocol, scheme and port.
    pub endpoints: Vec<Endpoint>,
}

/// Auth plugin binding of an upstream.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: Option<String>,
    /// Sharing mode for hierarchical merge.
    pub sharing: SharingMode,
    /// Plugin configuration.
    pub config: serde_json::Value,
}

/// Which inbound headers are forwarded upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward none (default; only configured `set`/`add` headers travel).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything except hop-by-hop and routing headers.
    All,
}

/// Request-header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestHeaderRules {
    /// Overwrite if present.
    pub set: BTreeMap<String, String>,
    /// Append (duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Remove from the forwarded set.
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    pub passthrough: Passthrough,
    /// Names forwarded when `passthrough == allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// Response-header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponseHeaderRules {
    /// Overwrite if present.
    pub set: BTreeMap<String, String>,
    /// Append (duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Strip before returning to the client.
    pub remove: Vec<String>,
}

/// `headers` block of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    pub request: RequestHeaderRules,
    /// Rules applied to the response returned to the client.
    pub response: ResponseHeaderRules,
}

/// Sustained rate component of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SustainedRate {
    /// Tokens replenished per `window`.
    pub rate: u64,
    /// Window unit.
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

/// Window unit of a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// One second.
    #[default]
    Second,
    /// Sixty seconds.
    Minute,
    /// 3600 seconds.
    Hour,
    /// 86 400 seconds.
    Day,
}

impl RateWindow {
    /// Window length in seconds.
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

/// Burst component of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Burst {
    /// Maximum bucket size.
    pub capacity: u64,
}

impl Default for Burst {
    fn default() -> Self {
        Self { capacity: 1 }
    }
}

/// Counter scope of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket shared by every caller.
    Global,
    /// One bucket per calling tenant (default).
    #[default]
    Tenant,
    /// One bucket per authenticated subject.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per matched route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429 (only strategy implemented).
    #[default]
    Reject,
    /// Queue the request (not implemented; refused at validation time).
    Queue,
    /// Degrade the response (not implemented; refused at validation time).
    Degrade,
}

/// Token-bucket rate-limit configuration (`ADR 0003`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical merge.
    pub sharing: SharingMode,
    /// `token_bucket` (default) or `sliding_window` (treated as token bucket).
    pub algorithm: RateAlgorithm,
    /// Sustained refill rate.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    pub burst: Option<Burst>,
    /// Counter scope.
    pub scope: RateScope,
    /// Overload strategy.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u64,
    /// Emit `X-RateLimit-*` headers.
    pub response_headers: bool,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[default]
    TokenBucket,
    /// Sliding window (treated as a token bucket with capacity == rate).
    SlidingWindow,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: SustainedRate::default(),
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
            response_headers: true,
        }
    }
}

impl RateLimitConfig {
    /// Sustained rate, reported as `X-RateLimit-Limit`.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.sustained.rate
    }

    /// Burst capacity, falling back to the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst
            .map_or(self.sustained.rate, |b| b.capacity.max(1))
    }

    /// Tokens replenished per second.
    #[must_use]
    pub fn tokens_per_second(&self) -> f64 {
        let window =
            f64::from(u32::try_from(self.sustained.window.secs().max(1)).unwrap_or(u32::MAX));
        f64::from(u32::try_from(self.sustained.rate).unwrap_or(u32::MAX)) / window
    }
}

/// CORS configuration (`ADR 0004`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Sharing mode for hierarchical merge.
    pub sharing: SharingMode,
    /// Enable CORS for this upstream/route.
    pub enabled: bool,
    /// Allowed origins; `*` allows any origin.
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    pub expose_headers: Vec<String>,
    /// Allow credentialed requests (incompatible with `*`).
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// Plugin chain binding. Accepts either a bare plugin identifier (GTS id or
/// custom-plugin UUID) or an object carrying `plugin_ref` plus `config`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBindingDto {
    /// `"<gts-identifier or uuid>"`.
    Ref(String),
    /// `{"plugin_ref": "...", "config": {...}}`.
    Detailed {
        /// Canonical plugin identifier.
        plugin_ref: String,
        /// Plugin configuration.
        #[serde(default)]
        config: serde_json::Value,
    },
}

/// `plugins` block of an upstream/route.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Sharing mode for hierarchical merge.
    pub sharing: SharingMode,
    /// Ordered plugin bindings; executed root → descendant, upstream → route.
    pub items: Vec<PluginBindingDto>,
}

/// HTTP method accepted by a route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// GET
    Get,
    /// POST
    Post,
    /// PUT
    Put,
    /// DELETE
    Delete,
    /// PATCH
    Patch,
}

impl HttpMethod {
    /// `http::Method` for this variant.
    #[must_use]
    pub fn to_http(self) -> http::Method {
        match self {
            Self::Get => http::Method::GET,
            Self::Post => http::Method::POST,
            Self::Put => http::Method::PUT,
            Self::Delete => http::Method::DELETE,
            Self::Patch => http::Method::PATCH,
        }
    }

    /// Parse an `http::Method` into this enum.
    ///
    /// `HEAD` is folded into `Get`: the schema has no `HEAD` variant, and a
    /// route that accepts `GET` must also serve its `HEAD` equivalent.
    #[must_use]
    pub fn from_http(method: &http::Method) -> Option<Self> {
        if method == http::Method::GET || method == http::Method::HEAD {
            Some(Self::Get)
        } else if method == http::Method::POST {
            Some(Self::Post)
        } else if method == http::Method::PUT {
            Some(Self::Put)
        } else if method == http::Method::DELETE {
            Some(Self::Delete)
        } else if method == http::Method::PATCH {
            Some(Self::Patch)
        } else {
            None
        }
    }
}

/// How the `{*path}` suffix of the proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject any suffix beyond the matched path.
    Disabled,
    /// Append the suffix to the route path (default).
    #[default]
    Append,
}

/// HTTP match rules of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpMatch {
    /// Allowed methods.
    pub methods: Vec<HttpMethod>,
    /// Path pattern (longest prefix wins).
    pub path: String,
    /// Query parameters allowed through; empty means none.
    pub query_allowlist: Vec<String>,
    /// How the proxy URL suffix is treated.
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::default(),
        }
    }
}

/// gRPC match rules of a route.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules; exactly one of `http`/`grpc` is set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteMatch {
    /// HTTP match rules.
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    pub grpc: Option<GrpcMatch>,
}

impl RouteMatch {
    /// The HTTP match, if this is an HTTP route.
    #[must_use]
    pub fn http(&self) -> Option<&HttpMatch> {
        self.http.as_ref()
    }

    /// Whether the route is a gRPC route.
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.grpc.is_some()
    }
}

/// An upstream: tenant-scoped root configuration object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key used by `/oagw/v1/proxy/{alias}/...`.
    pub alias: String,
    /// Disabled upstreams reject every request.
    pub enabled: bool,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Auth plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate-limit configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            alias: String::new(),
            enabled: true,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

/// A route belonging to an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream (immutable after creation).
    pub upstream_id: Uuid,
    /// Disabled routes are excluded from matching.
    pub enabled: bool,
    /// Flat tags for discovery.
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Plugin chain appended after the upstream chain.
    pub plugins: PluginsConfig,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS; when present it replaces the upstream's for this route
    /// (`ADR 0004` "Upstream/Route CORS Field").
    pub cors: Option<CorsConfig>,
    /// Selection precedence among routes that match the same pattern: the
    /// highest priority wins, and insertion order is the final tie-breaker.
    pub priority: i64,
    /// Monotonic insertion index, used as the final tie-breaker.
    #[serde(default, skip_serializing)]
    pub position: u64,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            enabled: true,
            tags: Vec::new(),
            match_rule: RouteMatch::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
            priority: 0,
            position: 0,
        }
    }
}

/// A tenant-defined custom plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomPlugin {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// `auth`, `guard`, or `transform`.
    pub plugin_type: String,
    /// Human-readable name.
    pub name: String,
    /// Sandboxed Starlark source (stored verbatim, never executed here).
    pub source_code: String,
    /// Declared configuration schema.
    pub config_schema: serde_json::Value,
    /// Set at insert time because the definition is unreferenced; a
    /// simplification, not a reclamation signal: nothing periodically sweeps
    /// unreferenced plugins out of the store (documented future work).
    pub gc_eligible: bool,
}

impl Default for CustomPlugin {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            plugin_type: "transform".to_owned(),
            name: String::new(),
            source_code: String::new(),
            config_schema: serde_json::Value::Object(serde_json::Map::new()),
            gc_eligible: false,
        }
    }
}

/// Strip an optional GTS instance prefix (`gts.cf.core.oagw.<x>.v1~<uuid>`)
/// and parse the remaining instance part as a UUID.
#[must_use]
pub fn parse_gts_uuid(raw: &str, base: &str) -> Option<Uuid> {
    let instance = raw.strip_prefix(base).unwrap_or(raw);
    Uuid::parse_str(instance.trim()).ok()
}

/// The instance part of a GTS identifier: everything after the first `~`.
///
/// Identifiers without a `~` are returned unchanged.
#[must_use]
pub fn gts_instance(identifier: &str) -> &str {
    identifier
        .split_once('~')
        .map_or(identifier, |(_, instance)| instance)
}

/// Whether a dot-separated identifier segment is a GTS version token
/// (`v1`, `v2`, `v12`, ...).
///
/// The one reducer every plugin-identifier reader shares: the trailing version
/// segment of `cf.core.oagw.required_headers.v2` is not part of the plugin's
/// short name, so validation, the catalogue, and the plugin registries all
/// resolve a versioned identifier to the same key.
#[must_use]
pub fn is_version_segment(segment: &str) -> bool {
    let Some(digits) = segment
        .strip_prefix('v')
        .or_else(|| segment.strip_prefix('V'))
    else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|digit| digit.is_ascii_digit())
}

/// The short name of a plugin identifier: its last non-version segment.
///
/// The instance part of a GTS identifier ends in its version
/// (`cf.core.oagw.noop.v1`), so the trailing version segment is dropped before
/// the last segment is read. This is also the key the plugin registries
/// resolve implementations under, and the same reduction
/// [`crate::domain::validation::plugin_registry_key`] performs on a binding
/// before a lookup.
#[must_use]
pub fn plugin_short_name(identifier: &str) -> String {
    let instance = gts_instance(identifier.trim());
    let mut segments: Vec<&str> = instance
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() > 1 && segments.last().is_some_and(|part| is_version_segment(part)) {
        segments.pop();
    }
    segments.pop().unwrap_or(instance).to_owned()
}

/// Render an anonymous GTS identifier for a resource instance.
#[must_use]
pub fn gts_id(base: &str, id: Uuid) -> String {
    format!("{base}{id}")
}
