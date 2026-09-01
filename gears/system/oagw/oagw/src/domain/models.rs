//! Domain entities and configuration contracts for the OAGW control plane.
//!
//! Every wire-facing shape mirrors `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` exactly: field names, `snake_case`
//! spelling, `default` values, and `additionalProperties: false` (modelled as
//! `#[serde(deny_unknown_fields)]`).
//!
//! The configuration sub-structures (`ServerConfig`, `HeadersConfig`,
//! `RateLimitConfig`, `CorsConfig`, `MatchConfig`, …) are shared between the
//! request DTOs (`api::rest::dto`) and the stored entities so that a single
//! definition carries the contract. They are plain data: no infrastructure or
//! transport type is referenced from here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use toolkit_gts::gts_id;
use uuid::Uuid;

use crate::domain::error::DomainError;

// ---------------------------------------------------------------------------
// GTS type identifiers
// ---------------------------------------------------------------------------

/// GTS type id of an upstream resource: `gts.cf.core.oagw.upstream.v1~`.
pub const UPSTREAM_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// GTS type id of a route resource: `gts.cf.core.oagw.route.v1~`.
pub const ROUTE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// GTS type id of a custom auth plugin: `gts.cf.core.oagw.auth_plugin.v1~`.
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// GTS type id of a custom guard plugin: `gts.cf.core.oagw.guard_plugin.v1~`.
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// GTS type id of a custom transform plugin:
/// `gts.cf.core.oagw.transform_plugin.v1~`.
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");
/// GTS id of the HTTP upstream protocol.
pub const HTTP_PROTOCOL: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// GTS id of the gRPC upstream protocol.
pub const GRPC_PROTOCOL: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// Standard port for plaintext HTTP endpoints (DESIGN §3.1 "Standard ports").
pub const HTTP_STANDARD_PORT: u16 = 80;
/// Standard port for TLS-based protocols (`https`/`wss`/`wt`/`grpc`).
pub const TLS_STANDARD_PORT: u16 = 443;

/// Renders the anonymous GTS identifier of a resource instance:
/// `gts.cf.core.oagw.<type>.v1~<uuid>`.
#[must_use]
pub fn gts_instance_id(type_id: &str, id: Uuid) -> String {
    format!("{type_id}{id}")
}

/// Extracts the instance UUID from an anonymous GTS identifier.
///
/// Accepts both the full identifier (`gts.cf.core.oagw.upstream.v1~<uuid>`)
/// and the bare UUID, so that callers may use whichever spelling is at hand.
#[must_use]
pub fn strip_gts_prefix<'a>(type_id: &str, raw: &'a str) -> Option<&'a str> {
    raw.strip_prefix(type_id).filter(|rest| !rest.is_empty())
}

/// Extracts the instance UUID from an anonymous GTS identifier of any of the
/// OAGW resource types.
#[must_use]
pub fn strip_gts_prefix_of_any(raw: &str) -> Option<String> {
    [
        UPSTREAM_TYPE,
        ROUTE_TYPE,
        AUTH_PLUGIN_TYPE,
        GUARD_PLUGIN_TYPE,
        TRANSFORM_PLUGIN_TYPE,
    ]
    .into_iter()
    .find_map(|type_id| strip_gts_prefix(type_id, raw).map(str::to_owned))
}

/// Whether `raw` matches the discovery-tag pattern `^[a-z0-9_-]+$`.
#[must_use]
pub fn tag_is_valid(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Transport scheme of a single upstream endpoint.
///
/// `http` is accepted at the type level because `gears.oagw.config
/// .allow_http_upstream` explicitly opts into plaintext upstreams for
/// non-production deployments; the domain validation rejects it otherwise
/// (DESIGN §2.2 `constraint-https-only`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndpointScheme {
    /// Plaintext HTTP — only valid when `allow_http_upstream` is enabled.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport over TLS.
    Wt,
    /// gRPC over HTTP/2 + TLS.
    Grpc,
}

impl EndpointScheme {
    /// Whether `port` is the scheme's standard port and therefore omitted
    /// from a derived alias.
    #[must_use]
    pub fn is_standard_port(self, port: u16) -> bool {
        match self {
            Self::Http => port == HTTP_STANDARD_PORT,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => port == TLS_STANDARD_PORT,
        }
    }

    /// Default port announced by the upstream schema.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => HTTP_STANDARD_PORT,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => TLS_STANDARD_PORT,
        }
    }

    /// Whether the scheme carries TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }

    /// Wire spelling of the scheme, as used by the JSON schemas.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// Protocol used to talk to the upstream service (`upstream.protocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum Protocol {
    /// Plain HTTP/1.1 and HTTP/2 request proxying.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC request proxying (planned for phase 3 — no proxy code path yet).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// GTS id of the protocol variant.
    #[must_use]
    pub fn gts_id(self) -> &'static str {
        match self {
            Self::Http => HTTP_PROTOCOL,
            Self::Grpc => GRPC_PROTOCOL,
        }
    }
}

/// A single upstream endpoint: `scheme` + `host` + `port`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Endpoint {
    /// Endpoint scheme; defaults to `https`.
    #[serde(default = "default_endpoint_scheme")]
    pub scheme: EndpointScheme,
    /// Hostname or IP literal of the upstream service.
    pub host: String,
    /// Endpoint port; defaults to the scheme's standard port.
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
}

fn default_endpoint_scheme() -> EndpointScheme {
    EndpointScheme::Https
}

fn default_endpoint_port() -> u16 {
    TLS_STANDARD_PORT
}

impl Endpoint {
    /// Builds an endpoint with explicit values.
    #[must_use]
    pub fn new(scheme: EndpointScheme, host: impl Into<String>, port: u16) -> Self {
        Self {
            scheme,
            host: host.into(),
            port,
        }
    }

    /// `host` when the port is the scheme's standard port, `host:port`
    /// otherwise.
    #[must_use]
    pub fn host_with_port(&self) -> String {
        if self.scheme.is_standard_port(self.port) {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `upstream.server`: the pool of endpoints backing one upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ServerConfig {
    /// At least one endpoint; all endpoints share `scheme` and `port`.
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical sharing mode (DESIGN §3.1 "Hierarchical Configuration").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

/// `upstream.auth`: authentication plugin binding.
///
/// The schema deliberately allows additional properties here, so this struct
/// is *not* `deny_unknown_fields` — plugin configuration payloads evolve
/// independently of the gateway release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct AuthConfig {
    /// Authentication plugin GTS identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Sharing mode for hierarchical inheritance.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Free-form plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Ordered list of plugin bindings attached to an upstream or route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct PluginsConfig {
    /// Sharing mode for the plugin chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Built-in plugins by GTS id, custom plugins by UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            items: Vec::new(),
        }
    }
}

/// `upstream.headers.request` / `upstream.headers.response` rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct HeaderRules {
    /// Headers to set (overwrite when present).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to drop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded to the upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<HeaderPassthrough>,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Inbound header forwarding policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeaderPassthrough {
    /// Drop every inbound header.
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything (hop-by-hop headers are still stripped).
    All,
}

/// `upstream.headers`: request and response transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<HeaderRules>,
    /// Rules applied to the response returned to the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<HeaderRules>,
}

/// Sustained request rate (tokens replenished per window).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SustainedRate {
    /// Tokens replenished per window; must be `>= 1`.
    pub rate: u64,
    /// Window length; defaults to `second`.
    #[serde(default = "default_rate_window")]
    pub window: RateWindow,
}

fn default_rate_window() -> RateWindow {
    RateWindow::Second
}

/// Time window used by a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
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
    pub fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Burst capacity of the token bucket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct BurstCapacity {
    /// Bucket capacity; defaults to the sustained rate.
    pub capacity: u64,
}

/// Rate-limit configuration shared by upstreams and routes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical inheritance.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Limiting algorithm; defaults to `token_bucket`.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate — the only required rate-limit field.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    /// Counting scope; defaults to `tenant`.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// Behaviour when the limit is exhausted; defaults to `reject`.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request; defaults to `1`.
    #[serde(default = "default_rate_cost")]
    pub cost: u64,
    /// Whether the limiter publishes `X-RateLimit-*` response headers
    /// (ADR 0003 "Configuration", `response_headers`, default `true`).
    #[serde(default = "default_response_headers")]
    pub response_headers: bool,
}

impl RateLimitConfig {
    /// Effective burst capacity: `burst.capacity` when set, else the
    /// sustained rate (schema default).
    #[must_use]
    pub fn effective_capacity(&self) -> u64 {
        self.burst
            .as_ref()
            .map_or(self.sustained.rate, |burst| burst.capacity)
    }
}

fn default_rate_cost() -> u64 {
    1
}

fn default_response_headers() -> bool {
    true
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Classic token bucket: allows bounded bursts.
    #[default]
    TokenBucket,
    /// Sliding window: prevents boundary bursts.
    SlidingWindow,
}

/// Scope of the rate-limit counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    /// One counter per deployment.
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

/// Behaviour when a rate limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// Reject with `429` + `Retry-After` (default).
    #[default]
    Reject,
    /// Queue the request until capacity is available.
    Queue,
    /// Degrade (serve a reduced response).
    Degrade,
}

/// CORS configuration for an upstream or route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct CorsConfig {
    /// Sharing mode for hierarchical inheritance.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled for this resource.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods; defaults to `GET`/`POST`.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<CorsMethod>,
    /// Headers exposed to browsers beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentialed requests; incompatible with `allowed_origins: ["*"]`.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<CorsMethod> {
    vec![CorsMethod::Get, CorsMethod::Post]
}

/// HTTP method allowed by a CORS configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum CorsMethod {
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
    /// `HEAD`
    Head,
    /// `OPTIONS`
    Options,
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// HTTP methods matched by an HTTP route (route schema enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH`
    Patch,
}

/// How `/{path_suffix}` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject any path suffix.
    Disabled,
    /// Append the suffix to `match.http.path` (default).
    #[default]
    Append,
}

/// HTTP match rules (`route.match.http`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct HttpMatch {
    /// Methods supported by this route; at least one.
    pub methods: Vec<HttpMethod>,
    /// Path pattern for the route (longest-prefix matched at proxy time).
    pub path: String,
    /// Allowed query parameters; empty means "none allowed".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// Suffix handling; defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (`match.grpc`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GrpcMatch {
    /// Fully qualified service name, e.g. `foo.v1.UserService`.
    pub service: String,
    /// RPC method name, e.g. `GetUser`.
    pub method: String,
}

/// Protocol-scoped match rules: exactly one of `http` / `grpc`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MatchConfig {
    /// HTTP match rules (upstream protocol `http`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules (upstream protocol `grpc`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// Which match family this configuration selects.
    #[must_use]
    pub fn kind(&self) -> MatchKind {
        if self.http.is_some() {
            MatchKind::Http
        } else {
            MatchKind::Grpc
        }
    }

    /// HTTP match rules, when this is an HTTP route.
    #[must_use]
    pub fn http(&self) -> Option<&HttpMatch> {
        self.http.as_ref()
    }
}

/// Match family of a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// `match.http`
    Http,
    /// `match.grpc`
    Grpc,
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// Tenant-scoped upstream configuration (`gts.cf.core.oagw.upstream.v1~`).
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key, unique per tenant; derived or operator-provided.
    pub alias: String,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Authentication plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain binding.
    pub plugins: Option<PluginsConfig>,
    /// Upstream-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Creation time (Unix epoch milliseconds).
    pub created_at: u64,
    /// Last modification time (Unix epoch milliseconds).
    pub updated_at: u64,
}

impl Upstream {
    /// Anonymous GTS identifier of this upstream.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts_instance_id(UPSTREAM_TYPE, self.id)
    }

    /// The alias, normalised (lowercase, trailing dots stripped).
    #[must_use]
    pub fn normalized_alias(&self) -> String {
        crate::domain::alias::normalize_alias(&self.alias)
    }

    /// Hosts of the endpoint pool, normalised.
    #[must_use]
    pub fn endpoint_hosts(&self) -> Vec<String> {
        self.server
            .endpoints
            .iter()
            .map(|endpoint| crate::domain::alias::normalize_host(&endpoint.host).unwrap_or_else(|_| endpoint.host.clone()))
            .collect()
    }
}

/// Route definition (`gts.cf.core.oagw.route.v1~`).
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Referenced upstream; immutable after creation.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Sort key for deterministic matching; routes with equal `(path,
    /// priority)` may not share methods.
    pub priority: i32,
    /// Match rules; immutable family (`http` xor `grpc`).
    pub match_config: MatchConfig,
    /// Route-level plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Creation time (Unix epoch milliseconds).
    pub created_at: u64,
    /// Last modification time (Unix epoch milliseconds).
    pub updated_at: u64,
}

impl Route {
    /// Anonymous GTS identifier of this route.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts_instance_id(ROUTE_TYPE, self.id)
    }
}

/// Plugin type (`plugin_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Credential injection (`auth_plugin`).
    Auth,
    /// Validation / policy enforcement (`guard_plugin`).
    Guard,
    /// Request/response mutation (`transform_plugin`).
    Transform,
}

impl PluginKind {
    /// GTS type prefix of this plugin family.
    #[must_use]
    pub fn type_id(self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_TYPE,
            Self::Guard => GUARD_PLUGIN_TYPE,
            Self::Transform => TRANSFORM_PLUGIN_TYPE,
        }
    }
}

/// Plugin lifecycle phase declared by a transform plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // intentional: the schema spells the phases `on_request`/`on_response`/`on_error`
pub enum PluginPhase {
    /// Runs before the upstream call.
    OnRequest,
    /// Runs after the upstream response.
    OnResponse,
    /// Runs on gateway or upstream failure.
    OnError,
}

/// Tenant-defined custom plugin (`oagw_plugin`).
///
/// The `plugin_type` field name is mandated by the plugin schema; it is kept
/// verbatim so the wire shape cannot drift from the specification.
///
/// Immutable after creation: updates create a new plugin and re-bind
/// references (DESIGN §3.1 "Plugin Lifecycle Management").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[allow(clippy::struct_field_names)] // intentional: `plugin_type` is the schema-mandated field name
pub struct Plugin {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin family.
    pub plugin_type: PluginKind,
    /// Unique name within the tenant.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Phases implemented by the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PluginPhase>,
    /// JSON Schema validating the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Sandboxed Starlark source.
    pub source_code: String,
    /// Creation time (Unix epoch milliseconds).
    pub created_at: u64,
    /// Last modification time (Unix epoch milliseconds).
    pub updated_at: u64,
    /// When the plugin was last resolved by the data plane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    /// When the plugin becomes eligible for garbage collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<u64>,
}

impl Plugin {
    /// Anonymous GTS identifier of this plugin.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts_instance_id(self.plugin_type.type_id(), self.id)
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validates the endpoint pool of an upstream.
///
/// Review evidence (security boundary — plaintext upstream opt-in):
/// * Guardrail: PRD §5.5 / DESIGN §3.1 "Standard ports" — gateway egress is
///   TLS-only unless the operator explicitly enables
///   `gears.oagw.config.allow_http_upstream`.
/// * Rationale: the endpoint pool is the only place where an upstream could
///   downgrade egress to plaintext, and the control plane is the only gate
///   every transport passes through, so the check lives here and defaults to
///   refusing.
/// * Validation performed: `upstream_plaintext_requires_opt_in` (REST) asserts
///   400 with the offending `host:port`, and `upstream_crud_round_trip` runs
///   with the opt-in enabled to prove the flag is honoured.
///
/// # Errors
///
/// Returns a [`DomainError::ValidationError`] when the pool is empty, when an
/// endpoint host is neither an RFC 1123 hostname nor an IP literal, or when
/// the endpoints do not share `scheme` and `port` (PRD §5.5
/// "Multi-Endpoint Pooling").
pub fn validate_endpoint_pool(
    endpoints: &[Endpoint],
    allow_http: bool,
) -> Result<(), DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::validation(
            "`server.endpoints` must contain at least one endpoint",
        ));
    }

    for endpoint in endpoints {
        if !allow_http && !endpoint.scheme.is_tls() {
            return Err(DomainError::validation_with_value(
                "plaintext upstream endpoints are not allowed: enable \
                 `gears.oagw.config.allow_http_upstream` to accept them",
                endpoint.host_with_port(),
            ));
        }
        crate::domain::alias::validate_endpoint_host(endpoint)?;
    }

    let first = &endpoints[0];
    if let Some(mismatch) = endpoints
        .iter()
        .find(|endpoint| endpoint.scheme != first.scheme || endpoint.port != first.port)
    {
        return Err(DomainError::validation_with_value(
            "all endpoints of an upstream must share the same scheme and port",
            format!(
                "{} does not match {}",
                mismatch.host_with_port(),
                first.host_with_port()
            ),
        ));
    }

    Ok(())
}

impl Endpoint {
    /// `scheme` as spelled in the upstream schema.
    #[must_use]
    pub fn scheme_label(&self) -> &'static str {
        match self.scheme {
            EndpointScheme::Http => "http",
            EndpointScheme::Https => "https",
            EndpointScheme::Wss => "wss",
            EndpointScheme::Wt => "wt",
            EndpointScheme::Grpc => "grpc",
        }
    }
}
