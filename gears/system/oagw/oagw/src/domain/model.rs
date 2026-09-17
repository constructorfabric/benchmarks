//! Domain model for oagw upstreams, routes and plugins.
//!
//! Field names mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` exactly; `http` is accepted as an
//! endpoint scheme (wire-contract override) in addition to the schema's
//! `https|wss|wt|grpc`.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

/// Wire `protocol` value for HTTP upstreams.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Wire `protocol` value for gRPC upstreams (no proxy path implemented).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Base GTS type of auth plugins.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Base GTS type of guard plugins.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Base GTS type of transform plugins.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Named (built-in) plugin identifiers, keyed by short name.
pub mod builtins {
    /// Base type of the plugin, e.g. `gts.cf.core.oagw.auth_plugin.v1~`.
    pub const AUTH: &str = super::AUTH_PLUGIN_TYPE;
    /// Base type of guard plugins.
    pub const GUARD: &str = super::GUARD_PLUGIN_TYPE;
    /// Base type of transform plugins.
    pub const TRANSFORM: &str = super::TRANSFORM_PLUGIN_TYPE;

    /// Named auth plugin ids.
    pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// Named auth plugin id.
    pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// Named auth plugin id.
    pub const AUTH_OAUTH2: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// Named auth plugin id.
    pub const AUTH_OAUTH2_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// Named guard plugin id.
    pub const GUARD_REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Named transform plugin id.
    pub const TRANSFORM_REQUEST_ID: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

    /// Catalog-only auth identifiers (no backing implementation).
    pub const CATALOG_ONLY: [&str; 6] = [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
    ];
}

/// Protocol used to reach an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, utoipa::ToSchema)]
pub enum UpstreamProtocol {
    /// Plain HTTP (SSE and WebSocket included).
    Http,
    /// gRPC — no proxy code path is implemented (DESIGN.md §3.3).
    Grpc,
}

impl UpstreamProtocol {
    /// The wire `protocol` value.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }

    /// Parses a wire `protocol` value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        if value == PROTOCOL_HTTP {
            Some(Self::Http)
        } else if value == PROTOCOL_GRPC {
            Some(Self::Grpc)
        } else {
            None
        }
    }
}

impl Serialize for UpstreamProtocol {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.gts_id())
    }
}

impl<'de> Deserialize<'de> for UpstreamProtocol {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown protocol: {value}")))
    }
}

/// Scheme of a single upstream endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, utoipa::ToSchema)]
pub enum EndpointScheme {
    /// Plaintext HTTP (accepted as a scheme; dialing is gated by
    /// `OagwConfig::allow_http_upstream`).
    Http,
    /// HTTPS.
    Https,
    /// Secure WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over HTTP/2.
    Grpc,
}

impl EndpointScheme {
    /// Lowercase wire name of the scheme.
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

    /// Parses a wire scheme name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "wss" => Some(Self::Wss),
            "wt" => Some(Self::Wt),
            "grpc" => Some(Self::Grpc),
            _ => None,
        }
    }

    /// Default port for the scheme (80 for http, 443 otherwise).
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            _ => 443,
        }
    }

    /// Whether the scheme speaks TLS.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }

    /// Standard port for the scheme — the port omitted from derived aliases.
    #[must_use]
    pub const fn is_standard_port(self, port: u16) -> bool {
        port == self.default_port()
    }
}

impl Serialize for EndpointScheme {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for EndpointScheme {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown endpoint scheme: {value}")))
    }
}

/// Hierarchical sharing mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum SharingMode {
    /// Not visible to descendants.
    #[serde(rename = "private")]
    #[default]
    Private,
    /// Descendants may override.
    #[serde(rename = "inherit")]
    Inherit,
    /// Descendants may not override.
    #[serde(rename = "enforce")]
    Enforce,
}

/// Which inbound request headers are forwarded upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum PassthroughMode {
    /// Forward nothing but the allowlist behaviour of the data plane.
    #[serde(rename = "none")]
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    #[serde(rename = "allowlist")]
    Allowlist,
    /// Forward everything (hop-by-hop headers are always stripped).
    #[serde(rename = "all")]
    All,
}

/// One upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// Endpoint scheme.
    pub scheme: EndpointScheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; `None` means the scheme's standard port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Endpoint {
    /// The port to dial: the explicit port when present, else the scheme's
    /// standard port.
    #[must_use]
    pub const fn effective_port(&self) -> u16 {
        match self.port {
            Some(port) => port,
            None => self.scheme.default_port(),
        }
    }

    /// Whether the endpoint dials the scheme's standard port.
    #[must_use]
    pub const fn is_standard_port(&self) -> bool {
        self.scheme.is_standard_port(self.effective_port())
    }
}

/// Endpoint pool of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServerConfig {
    /// At least one endpoint; all endpoints must share scheme and port.
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin configuration of an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier (named or UUID-backed).
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode of the auth configuration.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Auth plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

/// Header operations applied to a header collection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderOps {
    /// Headers to set (overwrite when present).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Request header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RequestHeaderOps {
    /// Set operations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Add operations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Remove operations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Allowlist used when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResponseHeaderOps {
    /// Set operations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Add operations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names stripped from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Request/response header transformation configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeadersConfig {
    /// Outbound request rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderOps>,
    /// Response rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderOps>,
}

/// Plugin bindings of an upstream or route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginsConfig {
    /// Sharing mode of the plugin chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin references: GTS ids of named plugins, UUIDs of custom plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
    /// Configuration of the bound plugins, keyed by their reference.
    ///
    /// The binding table the specification describes carries a `config`
    /// column next to `plugin_ref`; on the wire the same pairing may also
    /// travel with the binding itself, which [`PluginBinding`] accepts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<BTreeMap<String, Value>>,
}

impl PluginsConfig {
    /// The configuration a binding carries, preferring the inline form and
    /// falling back to the shared map.
    #[must_use]
    pub fn config_of(&self, reference: &str) -> Value {
        for item in &self.items {
            if item.reference() == reference
                && let Some(config) = item.inline_config()
            {
                return config.clone();
            }
        }
        self.config
            .as_ref()
            .and_then(|configs| configs.get(reference))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// One entry of [`PluginsConfig::items`].
///
/// ADR-0009 renders a binding as `{ "plugin_ref": …, "config": … }`, the
/// schemas as a bare GTS identifier; both are accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum PluginBinding {
    /// A bare reference.
    Reference(String),
    /// A reference with its own configuration.
    Detailed {
        /// The plugin reference.
        plugin_ref: String,
        /// The configuration the plugin reads through `ctx.config`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Value>,
    },
}

impl PluginBinding {
    /// The plugin reference of the binding.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            Self::Reference(reference) => reference,
            Self::Detailed { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The configuration carried by the binding itself, if any.
    #[must_use]
    pub const fn inline_config(&self) -> Option<&Value> {
        match self {
            Self::Reference(_) => None,
            Self::Detailed { config, .. } => config.as_ref(),
        }
    }
}

/// Sustained rate of a token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window length.
    #[serde(default)]
    pub window: RateWindow,
}

/// Window of a sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateWindow {
    /// One second.
    #[serde(rename = "second")]
    #[default]
    Second,
    /// One minute.
    #[serde(rename = "minute")]
    Minute,
    /// One hour.
    #[serde(rename = "hour")]
    Hour,
    /// One day.
    #[serde(rename = "day")]
    Day,
}

impl RateWindow {
    /// Length of the window in seconds.
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

/// Burst capacity of a token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BurstConfig {
    /// Bucket capacity; defaults to `sustained.rate`.
    pub capacity: u32,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateLimitAlgorithm {
    /// Token bucket with burst allowance.
    #[serde(rename = "token_bucket")]
    #[default]
    TokenBucket,
    /// Sliding window without boundary bursts.
    #[serde(rename = "sliding_window")]
    SlidingWindow,
}

/// Counter scope of a rate limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateScope {
    /// One counter for the whole gateway.
    #[serde(rename = "global")]
    Global,
    /// One counter per tenant.
    #[serde(rename = "tenant")]
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    #[serde(rename = "user")]
    User,
    /// One counter per client IP.
    #[serde(rename = "ip")]
    Ip,
    /// One counter per route.
    #[serde(rename = "route")]
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum RateStrategy {
    /// Reject with 429.
    #[serde(rename = "reject")]
    #[default]
    Reject,
    /// Queue the request (treated as reject in this build).
    #[serde(rename = "queue")]
    Queue,
    /// Degrade (treated as reject in this build).
    #[serde(rename = "degrade")]
    Degrade,
}

/// Rate limiting configuration (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitConfig {
    /// Sharing mode of the limit.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Rate limiting algorithm.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour when exhausted.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Whether `X-RateLimit-*` headers are emitted.
    #[serde(default = "crate::domain::model::default_true")]
    pub response_headers: bool,
    /// Tokens consumed per request.
    #[serde(default = "crate::domain::model::default_cost")]
    pub cost: u32,
}

fn default_true() -> bool {
    true
}

fn default_cost() -> u32 {
    1
}

impl RateLimitConfig {
    /// Effective bucket capacity: `burst.capacity` when set, else the
    /// sustained rate.
    #[must_use]
    pub const fn capacity(&self) -> u32 {
        match self.burst {
            Some(burst) => burst.capacity,
            None => self.sustained.rate,
        }
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CorsConfig {
    /// Sharing mode of the CORS configuration.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled for this resource.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods; defaults to `GET`/`POST`.
    #[serde(default = "crate::domain::model::default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed (incompatible with `["*"]`).
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// HTTP match rule of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpMatch {
    /// Methods this route accepts.
    pub methods: Vec<String>,
    /// Path prefix matched against the suffix of the proxy URL.
    pub path: String,
    /// Allowed query parameters; empty allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How the proxy URL's path suffix is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    #[serde(rename = "disabled")]
    Disabled,
    /// Append the suffix to the route path.
    #[serde(rename = "append")]
    #[default]
    Append,
}

impl PathSuffixMode {
    /// Whether the proxy URL's path suffix is appended to the route path.
    #[must_use]
    pub const fn is_append(self) -> bool {
        matches!(self, Self::Append)
    }
}

/// gRPC match rule of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules; exactly one of `http`/`grpc` is present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MatchConfig {
    /// HTTP match rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// Configuration of an upstream, as accepted on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[derive(utoipa::ToSchema)]
pub struct UpstreamSpec {
    /// Whether the upstream is reachable through the data plane.
    pub enabled: bool,
    /// Routing alias; derived from hostname endpoints when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol used to reach the upstream.
    pub protocol: UpstreamProtocol,
    /// Auth plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Guard/transform plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for UpstreamSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: Vec::new(),
            },
            protocol: UpstreamProtocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

/// Configuration of a route, as accepted on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[derive(utoipa::ToSchema)]
pub struct RouteSpec {
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_rule: MatchConfig,
    /// Guard/transform plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Route-level rate limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// Configuration of a custom plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
#[derive(utoipa::ToSchema)]
pub struct PluginSpec {
    /// Plugin kind: the GTS base type (`auth_plugin`/`guard_plugin`/
    /// `transform_plugin`).
    pub plugin_type: String,
    /// Human readable plugin name.
    pub name: String,
    /// Starlark source of the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// JSON schema of the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
}

impl Default for PluginSpec {
    fn default() -> Self {
        Self {
            plugin_type: TRANSFORM_PLUGIN_TYPE.to_owned(),
            name: String::new(),
            source_code: None,
            config_schema: None,
        }
    }
}

/// A persisted upstream: configuration plus identity.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// Bare UUID of the upstream.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Configuration.
    pub spec: UpstreamSpec,
}

impl Upstream {
    /// Resolved alias of the upstream.
    ///
    /// The control plane always derives or validates an alias before
    /// persisting, so this is empty only for hand-built values.
    #[must_use]
    pub fn alias(&self) -> &str {
        self.spec.alias.as_deref().unwrap_or("")
    }

    /// Whether any sharing mode of the configuration is `enforce`, which
    /// blocks descendant overrides of this upstream.
    #[must_use]
    pub fn enforces(&self) -> bool {
        [
            self.spec.auth.as_ref().map(|auth| auth.sharing),
            self.spec.plugins.as_ref().map(|plugins| plugins.sharing),
            self.spec.rate_limit.as_ref().map(|rate| rate.sharing),
            self.spec.cors.as_ref().map(|cors| cors.sharing),
        ]
        .into_iter()
        .flatten()
        .any(|sharing| matches!(sharing, SharingMode::Enforce))
    }

    /// GTS identifier of this upstream (`gts.cf.core.oagw.upstream.v1~{uuid}`).
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::gts_helpers::resource_id_to_gts(
            crate::gts_helpers::OagwResourceKind::Upstream,
            self.id,
        )
    }
}

/// A persisted route: configuration plus identity and upstream reference.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Bare UUID of the route.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Referenced upstream (same tenant).
    pub upstream_id: Uuid,
    /// Configuration.
    pub spec: RouteSpec,
}

/// A persisted custom plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    /// Bare UUID of the plugin.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Configuration.
    pub spec: PluginSpec,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn upstream_json() -> String {
        serde_json::json!({
            "enabled": true,
            "tags": ["llm"],
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "api.openai.com", "port": 443 }
                ]
            },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": builtins::AUTH_APIKEY,
                "sharing": "private",
                "config": { "header_name": "authorization" }
            },
            "headers": {
                "request": {
                    "set": { "x-trace": "1" },
                    "add": { "x-a": "b" },
                    "remove": ["x-secret"],
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["accept"]
                },
                "response": { "set": { "x-resp": "1" }, "remove": ["server"] }
            },
            "plugins": { "sharing": "private", "items": [builtins::GUARD_REQUIRED_HEADERS] },
            "rate_limit": {
                "sharing": "enforce",
                "algorithm": "token_bucket",
                "sustained": { "rate": 100, "window": "second" },
                "burst": { "capacity": 500 },
                "scope": "tenant",
                "strategy": "reject",
                "response_headers": true,
                "cost": 2
            },
            "cors": {
                "sharing": "private",
                "enabled": true,
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET", "POST"],
                "expose_headers": ["x-request-id"],
                "allow_credentials": true
            }
        })
        .to_string()
    }

    #[test]
    fn upstream_spec_parses_the_schema_field_names() {
        let spec: UpstreamSpec = serde_json::from_str(&upstream_json())
            .unwrap_or_else(|e| panic!("schema-shaped spec must parse: {e}"));
        assert!(spec.enabled);
        assert_eq!(spec.tags, vec!["llm".to_owned()]);
        assert_eq!(spec.server.endpoints.len(), 1);
        assert_eq!(spec.server.endpoints[0].scheme, EndpointScheme::Https);
        assert_eq!(spec.server.endpoints[0].host, "api.openai.com");
        assert_eq!(spec.server.endpoints[0].effective_port(), 443);
        assert_eq!(spec.protocol, UpstreamProtocol::Http);
        let auth = spec
            .auth
            .as_ref()
            .unwrap_or_else(|| panic!("auth must parse"));
        assert_eq!(auth.auth_type, builtins::AUTH_APIKEY);
        let headers = spec
            .headers
            .as_ref()
            .unwrap_or_else(|| panic!("headers must parse"));
        let request = headers
            .request
            .as_ref()
            .unwrap_or_else(|| panic!("request ops"));
        assert_eq!(request.passthrough, PassthroughMode::Allowlist);
        assert_eq!(request.set.get("x-trace"), Some(&"1".to_owned()));
        assert_eq!(request.remove, vec!["x-secret".to_owned()]);
        let rate = spec
            .rate_limit
            .as_ref()
            .unwrap_or_else(|| panic!("rate limit must parse"));
        assert_eq!(rate.sustained.rate, 100);
        assert_eq!(rate.sustained.window, RateWindow::Second);
        assert_eq!(rate.capacity(), 500);
        assert_eq!(rate.cost, 2);
        assert!(rate.response_headers);
        let cors = spec
            .cors
            .as_ref()
            .unwrap_or_else(|| panic!("cors must parse"));
        assert!(cors.enabled);
        assert!(cors.allow_credentials);
        assert_eq!(
            cors.allowed_origins,
            vec!["https://app.example.com".to_owned()]
        );
    }

    #[test]
    fn upstream_spec_round_trips() {
        let spec: UpstreamSpec = serde_json::from_str(&upstream_json())
            .unwrap_or_else(|e| panic!("spec must parse: {e}"));
        let rendered = serde_json::to_value(&spec).unwrap_or_default();
        let back: UpstreamSpec = serde_json::from_value(rendered)
            .unwrap_or_else(|e| panic!("rendered spec must reparse: {e}"));
        assert_eq!(spec, back);
    }

    #[test]
    fn endpoints_default_to_the_scheme_port() {
        let value = json!({
            "scheme": "https",
            "host": "api.openai.com"
        });
        let endpoint: Endpoint =
            serde_json::from_value(value).unwrap_or_else(|e| panic!("endpoint must parse: {e}"));
        assert_eq!(endpoint.effective_port(), 443);
        assert!(endpoint.is_standard_port());

        let http: Endpoint = serde_json::from_value(json!({ "scheme": "http", "host": "h" }))
            .unwrap_or_else(|e| panic!("http endpoint must parse: {e}"));
        assert_eq!(http.effective_port(), 80);
        assert!(!http.scheme.is_tls());
    }

    #[test]
    fn http_scheme_is_accepted_as_a_wire_scheme() {
        let spec: UpstreamSpec = serde_json::from_str(
            r#"{
                "server": { "endpoints": [ { "scheme": "http", "host": "10.0.0.1", "port": 8080 } ] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            }"#,
        )
        .unwrap_or_else(|e| panic!("http scheme must parse: {e}"));
        assert_eq!(spec.server.endpoints[0].scheme, EndpointScheme::Http);
        assert_eq!(spec.server.endpoints[0].effective_port(), 8080);
    }

    #[test]
    fn route_spec_parses_http_match() {
        let value = json!({
            "tags": ["chat"],
            "match": {
                "http": {
                    "methods": ["POST"],
                    "path": "/v1/chat",
                    "query_allowlist": ["model"],
                    "path_suffix_mode": "append"
                }
            },
            "plugins": { "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"] }
        });
        let spec: RouteSpec =
            serde_json::from_value(value).unwrap_or_else(|e| panic!("route spec must parse: {e}"));
        let http = spec
            .match_rule
            .http
            .as_ref()
            .unwrap_or_else(|| panic!("http match must parse"));
        assert_eq!(http.methods, vec!["POST".to_owned()]);
        assert_eq!(http.path, "/v1/chat");
        assert_eq!(spec.match_rule.grpc, None);
        assert_eq!(
            spec.plugins
                .as_ref()
                .unwrap_or_else(|| panic!("plugins must parse"))
                .items
                .len(),
            1
        );
    }

    #[test]
    fn route_spec_rejects_unknown_fields() {
        let value = json!({ "match": { "http": { "methods": ["GET"], "path": "/" } }, "bogus": 1 });
        assert!(
            serde_json::from_value::<RouteSpec>(value).is_err(),
            "deny_unknown_fields must reject unknown route fields"
        );
    }

    #[test]
    fn cors_defaults_follow_the_schema() {
        let cors: CorsConfig = serde_json::from_value(json!({ "enabled": true }))
            .unwrap_or_else(|e| panic!("minimal cors must parse: {e}"));
        assert_eq!(cors.allowed_methods.len(), 2);
        assert!(!cors.allow_credentials);
        assert_eq!(cors.sharing, SharingMode::Private);
    }

    #[test]
    fn rate_limit_defaults_follow_adr_0003() {
        let rate: RateLimitConfig = serde_json::from_value(json!({
            "sustained": { "rate": 10 }
        }))
        .unwrap_or_else(|e| panic!("rate limit must parse: {e}"));
        assert_eq!(rate.capacity(), 10);
        assert_eq!(rate.cost, 1);
        assert!(rate.response_headers);
        assert_eq!(rate.scope, RateScope::Tenant);
        assert_eq!(rate.strategy, RateStrategy::Reject);
    }

    #[test]
    fn unknown_protocol_is_rejected() {
        let err = serde_json::from_value::<UpstreamSpec>(json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "h" } ] },
            "protocol": "bogus"
        }));
        assert!(err.is_err(), "unknown protocol must be rejected");
    }
}
