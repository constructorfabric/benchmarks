//! Domain model for OAGW configuration resources.
//!
//! These types mirror the JSON schemas in `docs/schemas/`:
//! * [`Upstream`] — `upstream.v1.schema.json`
//! * [`Route`] — `route.v1.schema.json`
//! * [`Plugin`] — the custom-plugin shape from ADR-0002 Appendix A
//!
//! The structs are serde round-trippable end to end (request DTO == stored
//! entity == response DTO) and enforce `additionalProperties: false` via
//! `deny_unknown_fields`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::gts::PROTOCOL_GRPC_ID;

/// Sharing mode for configuration fields across the tenant hierarchy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Owner-only; descendants cannot see or override.
    #[default]
    Private,
    /// Visible to descendants, which may override where permitted.
    Inherit,
    /// Visible and forced on descendants; they cannot override.
    Enforce,
}

/// Endpoint scheme of an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// HTTPS.
    #[default]
    Https,
    /// Secure WebSocket.
    Wss,
    /// WebTransport (catalog only).
    Wt,
    /// gRPC (Phase 3).
    Grpc,
}

impl EndpointScheme {
    /// The default port for this scheme.
    #[must_use]
    pub fn default_port(&self) -> u16 {
        match self {
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Lowercased wire representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Scheme; defaults to `https`.
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; defaults to the scheme default.
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port (explicit or scheme default).
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// Effective port given the resolved wire `scheme`.
    ///
    /// When the endpoint declares no explicit port and the wire scheme has
    /// been downgraded to plain HTTP (`http`/`ws` — `allow_http_upstream`),
    /// the default port follows the wire scheme (80), not the configured one
    /// (443): downgraded plaintext traffic must never be sent to the TLS
    /// port. An explicit port always wins.
    #[must_use]
    pub fn effective_port_with_scheme(&self, scheme: &str) -> u16 {
        if self.port.is_none() && matches!(scheme, "http" | "ws") {
            80
        } else {
            self.effective_port()
        }
    }
}

/// Server configuration (endpoint pool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// At least one endpoint.
    pub endpoints: Vec<Endpoint>,
}

/// Upstream auth plugin binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Sharing mode for the hierarchy (default `inherit`).
    #[serde(default = "default_inherit")]
    pub sharing: SharingMode,
    /// Plugin configuration (keys depend on the plugin type).
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// Set (overwrite) headers on the outbound request.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append headers to the outbound request.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Remove headers from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<PassthroughMode>,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// Set (overwrite) headers on the outbound response.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append headers to the outbound response.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Remove headers from the upstream response.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Inbound header passthrough policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound headers.
    #[default]
    None,
    /// Forward only the allowlisted inbound headers.
    Allowlist,
    /// Forward all inbound headers.
    All,
}

/// Header transformation rules for requests and responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(default)]
    pub request: Option<RequestHeaders>,
    /// Response-side rules.
    #[serde(default)]
    pub response: Option<ResponseHeaders>,
}

/// A referenced plugin plus its per-plugin configuration (ADR-0009 wire
/// shape: `{ "plugin_ref": "<gts id>", "config": { ... } }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginRef {
    /// GTS identifier of the plugin (built-in id or custom plugin id).
    #[serde(rename = "plugin_ref")]
    pub plugin_ref: String,
    /// Per-plugin configuration keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// A single plugin-chain entry.
///
/// The wire accepts both the ADR-0009 object form and the earlier bare
/// GTS-id string (backward compatible); `With` entries carry per-plugin
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PluginBindingItem {
    /// Bare GTS id (legacy wire form; no per-plugin configuration).
    Bare(String),
    /// Reference plus per-plugin configuration (ADR-0009).
    With(PluginRef),
}

/// Empty per-plugin configuration for bare references.
static EMPTY_PLUGIN_CONFIG: BTreeMap<String, serde_json::Value> = BTreeMap::new();

impl PluginBindingItem {
    /// The resolved plugin reference and its configuration.
    #[must_use]
    pub fn as_ref(&self) -> (&str, &BTreeMap<String, serde_json::Value>) {
        match self {
            Self::Bare(reference) => (reference, &EMPTY_PLUGIN_CONFIG),
            Self::With(r) => (&r.plugin_ref, &r.config),
        }
    }
}

const fn default_inherit() -> SharingMode {
    SharingMode::Inherit
}

/// Binding of plugins (built-in GTS ids and/or custom plugin UUIDs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsBinding {
    /// Sharing mode for the plugin chain (default `inherit`: visible to
    /// descendants, which may extend it).
    #[serde(default = "default_inherit")]
    pub sharing: SharingMode,
    /// Plugin references in execution order (GTS id or `{plugin_ref, config}`).
    #[serde(default)]
    pub items: Vec<PluginBindingItem>,
}

impl Default for PluginsBinding {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Inherit,
            items: Vec::new(),
        }
    }
}

/// Sustained rate component of the token bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window unit.
    #[serde(default)]
    pub window: RateWindow,
}

/// Rate window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// Per second.
    #[default]
    Second,
    /// Per minute.
    Minute,
    /// Per hour.
    Hour,
    /// Per day.
    Day,
}

impl RateWindow {
    /// Duration of one window.
    #[must_use]
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Burst capacity component of the token bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BurstCapacity {
    /// Maximum bucket capacity.
    pub capacity: u64,
}

/// Token-bucket rate limiter counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket for the whole gateway.
    Global,
    /// One bucket per calling tenant.
    #[default]
    Tenant,
    /// One bucket per calling subject.
    User,
    /// One bucket per source IP.
    Ip,
    /// One bucket per matched route.
    Route,
}

/// Rate limiter strategy when a bucket is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    /// Queue the request (MVP: treated as reject-shaped pass, see report).
    Queue,
    /// Degrade (MVP: allow the request through).
    Degrade,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[default]
    TokenBucket,
    /// Sliding window (catalog only in MVP).
    SlidingWindow,
}

/// Rate limiting configuration (upstream or route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sharing mode for the hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm; default token bucket.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default)]
    pub burst: Option<BurstCapacity>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Behavior when exhausted.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "one")]
    pub cost: u64,
}

const fn one() -> u64 {
    1
}

impl RateLimit {
    /// Effective bucket capacity (burst or sustained rate).
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst
            .as_ref()
            .map_or(self.sustained.rate, |b| b.capacity)
    }
}

/// CORS configuration (upstream or route).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode for the hierarchy (default `inherit`).
    #[serde(default = "default_inherit")]
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any (no credentials).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods on actual requests.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Response headers exposed to browsers.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Inherit,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// An upstream (managed resource).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Server-generated GTS instance id (read-only on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the upstream is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing alias; auto-derived for hostname endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags (additive across the hierarchy).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol GTS id.
    pub protocol: String,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsBinding>,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    /// Stored form helper: whether the protocol is gRPC.
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.protocol == PROTOCOL_GRPC_ID
    }

    /// Whether any aligned capability is shared as `private` (making this
    /// upstream opaque to descendants for that capability).
    #[must_use]
    pub fn has_private_sharing(&self) -> bool {
        self.auth
            .as_ref()
            .is_some_and(|a| a.sharing == SharingMode::Private)
            || self
                .plugins
                .as_ref()
                .is_some_and(|p| p.sharing == SharingMode::Private)
            || self
                .cors
                .as_ref()
                .is_some_and(|c| c.sharing == SharingMode::Private)
            || self
                .rate_limit
                .as_ref()
                .is_some_and(|r| r.sharing == SharingMode::Private)
    }
}

const fn default_true() -> bool {
    true
}

/// HTTP method allowed on a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// DELETE.
    Delete,
    /// PATCH.
    Patch,
}

impl HttpMethod {
    /// Compare with an uppercase `http::Method` string.
    #[must_use]
    pub fn matches(&self, method: &str) -> bool {
        self.as_str().eq_ignore_ascii_case(method)
    }

    /// Wire representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// Path-suffix behavior for HTTP route matching.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the suffix to the route path (default).
    #[default]
    Append,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Allowed methods (at least one).
    pub methods: Vec<HttpMethod>,
    /// Path prefix pattern.
    pub path: String,
    /// Allowed query parameter names; empty means allow none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Path suffix behavior.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (Phase 3 catalog; no proxy code path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Route match rule (exactly one of `http` / `grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MatchRule {
    /// HTTP match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A route (managed resource).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Server-generated GTS instance id (read-only on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Whether the route is enabled (disabled routes are not selected).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit selection priority (higher wins; default 0).
    #[serde(default)]
    pub priority: u64,
    /// Owning upstream id (UUID or GTS id).
    #[serde(rename = "upstream_id")]
    pub upstream_id: String,
    /// Match rule.
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    /// Plugin chain binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsBinding>,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS (route-level override, per DESIGN domain model / ADR-0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// Kind of a custom plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Auth plugin.
    Auth,
    /// Guard plugin.
    Guard,
    /// Transform plugin.
    Transform,
}

impl PluginKind {
    /// GTS base type id for this kind.
    #[must_use]
    pub fn type_id(self) -> &'static str {
        match self {
            Self::Auth => crate::gts::AUTH_PLUGIN_TYPE_ID,
            Self::Guard => crate::gts::GUARD_PLUGIN_TYPE_ID,
            Self::Transform => crate::gts::TRANSFORM_PLUGIN_TYPE_ID,
        }
    }
}

/// A custom (Starlark) plugin definition.
///
/// Immutable after creation: no PUT endpoint exists for plugins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    /// Server-generated GTS instance id (read-only on the wire).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Short human-readable name.
    pub name: String,
    /// Optional free-form description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Plugin kind (auth / guard / transform).
    #[serde(rename = "plugin_type")]
    pub kind: PluginKind,
    /// JSON Schema describing the accepted `config` keys.
    pub config_schema: serde_json::Value,
    /// The Starlark source.
    #[serde(rename = "source_code")]
    pub source_code: String,
}

/// Stored upstream record (managed metadata + entity).
#[derive(Debug, Clone)]
pub struct UpstreamRecord {
    /// Owning tenant (server-assigned).
    pub tenant_id: Uuid,
    /// Managed entity.
    pub entity: Upstream,
}

/// Stored route record.
#[derive(Debug, Clone)]
pub struct RouteRecord {
    /// Owning tenant (server-assigned).
    pub tenant_id: Uuid,
    /// Managed entity.
    pub entity: Route,
}

/// Stored plugin record.
#[derive(Debug, Clone)]
pub struct PluginRecord {
    /// Owning tenant (server-assigned).
    pub tenant_id: Uuid,
    /// Managed entity.
    pub entity: Plugin,
}
