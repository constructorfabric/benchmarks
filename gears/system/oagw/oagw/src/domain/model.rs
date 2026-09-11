//! Domain model for upstreams, routes, plugins and their sub-configurations.
//!
//! The `Serialize`/`Deserialize` shapes here are the wire contract from
//! `docs/schemas/upstream.v1.schema.json` and `docs/schemas/route.v1.schema.json`;
//! unknown fields are rejected on the way in.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Failure modes produced by the domain layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// A request body or configuration value violated its contract.
    Invalid(String),
    /// No entity exists for the requested key.
    NotFound { kind: String, target: String },
    /// The entity exists but conflicts with another one.
    Conflict(String),
    /// An internal failure outside the domain contract.
    Internal(String),
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid: {msg}"),
            Self::NotFound { kind, target } => write!(f, "no {kind} for {target}"),
            Self::Conflict(msg) => write!(f, "conflict: {msg}"),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}

impl std::error::Error for DomainError {}

/// Endpoint transport scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
}

impl EndpointScheme {
    /// The scheme default applied when the endpoint field is omitted.
    #[must_use]
    pub fn default_scheme() -> Self {
        Self::Https
    }

    /// Whether the port is this scheme's standard (default) port.
    #[must_use]
    pub fn is_standard_port(self, port: u16) -> bool {
        match self {
            Self::Http => port == 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => port == 443,
        }
    }

    /// The scheme's standard port.
    #[must_use]
    pub fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether opening a connection in this scheme requires TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }

    /// The scheme as it appears on the wire.
    #[must_use]
    pub fn as_wire_scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Wss => "wss",
            Self::Https | Self::Wt | Self::Grpc => "https",
        }
    }
}

/// Upstream protocol, carried as its GTS identifier on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// Plain request/response HTTP.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC (accepted but not proxied in this delivery).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// The protocol's GTS identifier as it appears on the wire.
    #[must_use]
    pub fn as_gts_id(self) -> &'static str {
        match self {
            Self::Http => crate::gts_helpers::PROTOCOL_HTTP,
            Self::Grpc => crate::gts_helpers::PROTOCOL_GRPC,
        }
    }
}

/// One addressable server of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Transport scheme; defaults to `https`.
    #[serde(default = "EndpointScheme::default_scheme")]
    pub scheme: EndpointScheme,
    /// RFC 1123 hostname, or an IPv4/IPv6 literal.
    pub host: String,
    /// Port; defaults to 443.
    #[serde(default = "Endpoint::default_port")]
    pub port: u16,
}

impl Endpoint {
    fn default_port() -> u16 {
        443
    }

    /// Host with one trailing dot stripped, lowercased.
    #[must_use]
    pub fn normalised_host(&self) -> String {
        let trimmed = self.host.trim();
        let trimmed = trimmed.strip_suffix('.').unwrap_or(trimmed);
        trimmed.to_ascii_lowercase()
    }

    /// The `Host` value this endpoint answers as: the host, with the port
    /// appended whenever it is not the scheme's standard one.
    #[must_use]
    pub fn authority(&self) -> String {
        let host = self.normalised_host();
        if self.scheme.is_standard_port(self.port) {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// The `server` section of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// One or more endpoints, homogeneous in scheme and port.
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical sharing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// Credential-injection configuration carried on an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Sharing mode for hierarchical merging.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Opaque configuration read by the selected plugin.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Passthrough policy for inbound request headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// No inbound headers are forwarded.
    #[default]
    None,
    /// Only the allowlisted headers are forwarded.
    Allowlist,
    /// All inbound headers are forwarded.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers set (overwritten if present).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers added (may repeat).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names removed from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers set on the response returned to the client.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers added to the response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names stripped from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation configuration for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    #[serde(default, skip_serializing_if = "RequestHeaderRules::is_empty")]
    pub request: RequestHeaderRules,
    /// Rules applied to the response.
    #[serde(default, skip_serializing_if = "ResponseHeaderRules::is_empty")]
    pub response: ResponseHeaderRules,
}

impl RequestHeaderRules {
    /// Whether no request rule is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.passthrough == PassthroughMode::None
            && self.passthrough_allowlist.is_empty()
    }
}

impl ResponseHeaderRules {
    /// Whether no response rule is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.add.is_empty() && self.remove.is_empty()
    }
}

/// Time window for a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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
    /// Window length as a `std::time::Duration`.
    // The `from_mins`/`from_hours`/`from_days` constructors are still unstable.
    #[allow(clippy::duration_suboptimal_units)]
    #[must_use]
    pub fn duration(self) -> std::time::Duration {
        match self {
            Self::Second => std::time::Duration::from_secs(1),
            Self::Minute => std::time::Duration::from_secs(60),
            Self::Hour => std::time::Duration::from_secs(3_600),
            Self::Day => std::time::Duration::from_secs(86_400),
        }
    }
}

/// Sustained rate configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window for the sustained rate.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    /// Bucket capacity; defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Rate-limit behaviour when the bucket is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with `429`.
    #[default]
    Reject,
    /// Queue the request (degrades to `reject` in this delivery).
    Queue,
    /// Serve a degraded response (degrades to `reject` in this delivery).
    Degrade,
}

/// Counter scope for a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One shared bucket.
    Global,
    /// One bucket per tenant.
    #[default]
    Tenant,
    /// One bucket per principal.
    User,
    /// One bucket per client address.
    Ip,
    /// One bucket per route.
    Route,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket with burst capacity.
    #[default]
    TokenBucket,
    /// Sliding window (treated as a token bucket in this delivery).
    SlidingWindow,
}

/// Rate-limiting configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sharing mode for hierarchical merging.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm; only the token bucket is enforced.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate; required.
    pub sustained: SustainedRate,
    /// Burst capacity.
    #[serde(default)]
    pub burst: Burst,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour when the bucket is empty.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "RateLimit::default_cost")]
    pub cost: u32,
}

impl RateLimit {
    fn default_cost() -> u32 {
        1
    }

    /// Effective bucket capacity, defaulting to the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate).max(1)
    }

    /// Effective refill interval.
    #[must_use]
    pub fn refill_interval(&self) -> std::time::Duration {
        self.sustained.window.duration() / self.sustained.rate.max(1)
    }
}

/// Cross-origin resource sharing configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cors {
    /// Sharing mode for hierarchical merging.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS processing is enabled.
    pub enabled: bool,
    /// Allowed origins; `*` matches any.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods; defaults to `GET` and `POST`.
    #[serde(default = "Cors::default_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_credentials: bool,
}

impl Cors {
    /// A CORS block that is disabled and allows nothing.
    #[must_use]
    pub fn unset() -> Self {
        Self {
            sharing: SharingMode::Inherit,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    fn default_methods() -> Vec<String> {
        vec!["GET".to_owned(), "POST".to_owned()]
    }

    /// Whether the given origin is allowed.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
    }

    /// Whether the given method is allowed.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// A reference to a plugin, built-in by GTS id or custom by UUID.
///
/// The UUID variant is tried before the bare string: a GTS identifier is a
/// string that is simply not a UUID, so the order decides which shape a wire
/// value takes. The configured shape is tried first of all, since an object
/// cannot be mistaken for either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginRef {
    /// A plugin reference carrying its own configuration.
    Configured {
        /// The canonical plugin identifier.
        plugin_ref: String,
        /// The configuration object the plugin reads.
        #[serde(default)]
        config: serde_json::Value,
    },
    /// A custom plugin UUID.
    Uuid(Uuid),
    /// A GTS identifier (built-in or custom plugin type id).
    GtsId(String),
}

impl PluginRef {
    /// The plugin identifier the registry resolves.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::Configured { plugin_ref, .. } => plugin_ref.clone(),
            Self::Uuid(uuid) => uuid.to_string(),
            Self::GtsId(id) => id.clone(),
        }
    }

    /// The configuration object the plugin reads, empty when unconfigured.
    #[must_use]
    pub fn config(&self) -> serde_json::Value {
        match self {
            Self::Configured { config, .. } => config.clone(),
            _ => serde_json::Value::Null,
        }
    }
}

/// Plugin chain configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Sharing mode for hierarchical merging.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered plugin references.
    #[serde(default)]
    pub items: Vec<PluginRef>,
}

/// Plugin kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Credential injection.
    Auth,
    /// Validation and policy enforcement.
    Guard,
    /// Request and response mutation.
    Transform,
}

impl PluginKind {
    /// The GTS type prefix for this kind.
    #[must_use]
    pub fn type_prefix(self) -> &'static str {
        match self {
            Self::Auth => crate::gts_helpers::AUTH_PLUGIN_TYPE,
            Self::Guard => crate::gts_helpers::GUARD_PLUGIN_TYPE,
            Self::Transform => crate::gts_helpers::TRANSFORM_PLUGIN_TYPE,
        }
    }
}

/// Phase a plugin participates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// The variants carry the wire names (`on_request`/`on_response`) documented in
// `docs/DESIGN.md`, so the shared prefix is deliberate.
#[allow(clippy::enum_variant_names)]
pub enum PluginPhase {
    /// Called before the upstream call.
    OnRequest,
    /// Called with the upstream response.
    OnResponse,
    /// Called when the upstream call fails.
    OnError,
}

/// A custom plugin resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// `plugin_type` is the field name the management API documents.
#[allow(clippy::struct_field_names)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// System-generated identifier; assigned by the service.
    #[serde(default)]
    pub id: String,
    /// Owning tenant; assigned from the security context.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Kind of plugin.
    #[serde(rename = "plugin_type")]
    pub plugin_type: PluginKind,
    /// Name, unique per tenant.
    pub name: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional JSON Schema for the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Stored source text.
    pub source_code: String,
    /// Phases the plugin participates in.
    #[serde(default)]
    pub phases: Vec<PluginPhase>,
}

impl Plugin {
    /// The plugin's GTS-style identifier.
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", self.plugin_type.type_prefix(), self.id)
    }
}

/// A tenant-scoped description of one external service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// System-generated identifier; assigned by the service.
    #[serde(default)]
    pub id: String,
    /// Owning tenant; assigned from the security context.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Routing identifier, normalised to lowercase; derived from the endpoint
    /// pool when a create omits it.
    #[serde(default)]
    pub alias: String,
    /// Whether the upstream accepts proxy traffic.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// Protocol used to connect.
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Credential injection configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation timestamp (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last update timestamp (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Upstream {
    /// Whether no header or plugin configuration is present.
    #[must_use]
    pub fn is_new(&self) -> bool {
        self.created_at.is_none()
    }
}

impl HeadersConfig {
    /// Whether no header rule is configured at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.response.is_empty()
    }
}

impl PluginsConfig {
    /// Whether no plugin is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// HTTP matching rules for a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods the route accepts.
    pub methods: Vec<HttpMethod>,
    /// Path prefix the route matches.
    pub path: String,
    /// Query parameters the route forwards; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// Methods a route may accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Parses an inbound method name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }

    /// The method name on the wire.
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

/// How the proxy path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// A path suffix is rejected.
    Disabled,
    /// The path suffix is appended to the route path.
    #[default]
    Append,
}

/// gRPC matching rules for a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// The `match` section of a route: exactly one variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP matching rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC matching rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A rule mapping an inbound request onto an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// System-generated identifier; assigned by the service.
    #[serde(default)]
    pub id: String,
    /// Owning tenant; assigned from the security context.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Referenced upstream, immutable after create. The create path supplies
    /// it, so a body that omits it still deserialises.
    #[serde(default)]
    pub upstream_id: String,
    /// Whether the route participates in matching.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// Priority; higher wins on equal prefix length.
    #[serde(default)]
    pub priority: i32,
    /// Matching rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
    /// Plugin chain, appended after the upstream's.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation timestamp (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last update timestamp (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl Route {
    /// The HTTP match, when this is an HTTP route.
    #[must_use]
    pub fn http_match(&self) -> Option<&HttpMatch> {
        self.match_config.http.as_ref()
    }

    /// The gRPC match, when this is a gRPC route.
    #[must_use]
    pub fn grpc_match(&self) -> Option<&GrpcMatch> {
        self.match_config.grpc.as_ref()
    }
}

/// Default value for the `enabled` flag.
#[must_use]
pub fn default_true() -> bool {
    true
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
