//! Domain models for OAGW upstreams, routes and plugins.
//!
//! The wire shape mirrors the GTS schemas published in
//! `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` (camelCase, `additionalProperties:
//! false`). Internal bookkeeping fields (`id`, `tenant_id`) are marked
//! `#[serde(skip)]` so management-plane responses stay byte-compatible
//! with the documented schemas — `id` is server-assigned and surfaced
//! separately by the REST layer, never echoed inside the entity body.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::gts_helpers;

// ---------------------------------------------------------------------------
// Shared configuration enums
// ---------------------------------------------------------------------------

/// Hierarchical sharing mode for per-tenant configuration blocks.
///
/// * `private` — visible only to the owning tenant.
/// * `inherit` — visible to descendants, which may override it.
/// * `enforce` — pinned by the owning tenant; descendants cannot override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "camelCase")]
pub enum SharingMode {
    #[default]
    Private,
    Inherit,
    Enforce,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Time window of a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

/// Scope of rate-limit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Behaviour when a rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// How a route treats the `/{path_suffix}` of the proxy URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    Disabled,
    #[default]
    Append,
}

/// Which inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    #[default]
    None,
    Allowlist,
    All,
}

/// Upstream endpoint protocol scheme.
///
/// `Http` is not part of the published GTS schema (which lists
/// `https`/`wss`/`wt`/`grpc`) but is accepted for control-plane
/// validation when the gear is configured with `allow_http_upstream`;
/// the service layer rejects it otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    Https,
    Wss,
    Wt,
    Grpc,
    Http,
}

impl EndpointScheme {
    /// Default port for this scheme (schema default is `443`; `http`
    /// uses the conventional `80`).
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            _ => 443,
        }
    }
}

/// Which HTTP methods a route matches.
///
/// The wire form is uppercase (`"GET"` / `"POST"` / ...) per the upstream
/// schema's `cors.allowed_methods` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum CorsMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

/// HTTP method of an inbound route match.
///
/// The wire form is uppercase (`"GET"` / `"POST"` / ...) per the route
/// schema. Converted to/from `http::Method` at the data plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Connect,
    Trace,
}

impl HttpMethod {
    /// Map an `http::Method` onto the schema's method set.
    #[must_use]
    pub fn from_http_method(m: &http::Method) -> Option<Self> {
        Some(match *m {
            http::Method::GET => Self::Get,
            http::Method::POST => Self::Post,
            http::Method::PUT => Self::Put,
            http::Method::PATCH => Self::Patch,
            http::Method::DELETE => Self::Delete,
            http::Method::HEAD => Self::Head,
            http::Method::OPTIONS => Self::Options,
            http::Method::CONNECT => Self::Connect,
            http::Method::TRACE => Self::Trace,
            _ => return None,
        })
    }

    /// Convert back to an `http::Method` for forwarding.
    #[must_use]
    pub fn as_http_method(self) -> http::Method {
        match self {
            Self::Get => http::Method::GET,
            Self::Post => http::Method::POST,
            Self::Put => http::Method::PUT,
            Self::Patch => http::Method::PATCH,
            Self::Delete => http::Method::DELETE,
            Self::Head => http::Method::HEAD,
            Self::Options => http::Method::OPTIONS,
            Self::Connect => http::Method::CONNECT,
            Self::Trace => http::Method::TRACE,
        }
    }
}

impl CorsMethod {
    /// Default method set from the schema (`[GET, POST]`).
    #[must_use]
    pub fn default_set() -> Vec<Self> {
        vec![Self::Get, Self::Post]
    }

    /// Uppercase wire representation (`"GET"`, ...).
    #[must_use]
    pub fn as_str(self) -> &'static str {
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

// ---------------------------------------------------------------------------
// Endpoint / server
// ---------------------------------------------------------------------------

/// A single upstream endpoint (scheme + host + port).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Endpoint {
    pub scheme: EndpointScheme,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Port with the scheme default applied.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// Whether this endpoint uses the scheme's default port.
    #[must_use]
    pub fn on_default_port(&self) -> bool {
        self.port.is_none() || self.port == Some(self.scheme.default_port())
    }
}

/// Upstream server block (`server.endpoints`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

/// Upstream connection protocol (GTS instance identifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UpstreamProtocol {
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Upstream authentication configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthConfig {
    /// Auth plugin type — GTS identifier of the plugin instance
    /// (e.g. `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    #[serde(rename = "type")]
    pub plugin_type: String,
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration, interpreted by the resolved
    /// plugin implementation.
    #[serde(default)]
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Sustained rate (`sustained.rate` + `sustained.window`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SustainedRate {
    pub rate: u32,
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// Burst block (`burst.capacity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BurstConfig {
    #[serde(rename = "capacity")]
    pub capacity: u32,
}

/// Rate-limit configuration (shared by upstreams and routes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_cost")]
    pub cost: u32,
}

fn default_cost() -> u32 {
    1
}

/// Resolved rate-limit parameters: `rate` per `window` plus burst
/// capacity. Used by the enforcement machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRateLimit {
    /// Tokens replenished per `window`.
    pub rate: u32,
    /// Window duration in seconds.
    pub window_secs: u64,
    /// Bucket capacity (burst).
    pub capacity: u32,
    /// Tokens consumed per request.
    pub cost: u32,
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// CORS configuration (shared by upstreams and routes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    pub enabled: bool,
    /// Allowed origins: the literal `"*"` or absolute URIs.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods; defaults to `[GET, POST]`.
    #[serde(default = "CorsMethod::default_set")]
    pub allowed_methods: Vec<CorsMethod>,
    /// Headers exposed to browsers beyond the CORS-safelisted set.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

// ---------------------------------------------------------------------------
// Header transforms
// ---------------------------------------------------------------------------

/// Request header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RequestHeaderRules {
    /// Headers overwritten (matching existing values replaced).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers appended (existing values kept, new value added).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names stripped from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward to the upstream.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough == "allowlist"`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResponseHeaderRules {
    /// Headers overwritten on the upstream response.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers appended to the upstream response.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names stripped from the upstream response.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation block (`headers` on an upstream).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeaderRules,
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Kind of a plugin: auth, guard or transform (GTS type id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    /// GTS type identifier for this plugin kind.
    #[must_use]
    pub fn gts_type_id(self) -> &'static str {
        match self {
            Self::Auth => gts_helpers::AUTH_PLUGIN_TYPE_ID,
            Self::Guard => gts_helpers::GUARD_PLUGIN_TYPE_ID,
            Self::Transform => gts_helpers::TRANSFORM_PLUGIN_TYPE_ID,
        }
    }
}

/// A plugin reference inside a binding chain: either a builtin plugin
/// GTS identifier or the UUID of a stored custom plugin.
///
/// On the wire both are plain strings (the schema's oneOf); the
/// discriminant is structural — parsable as a UUID → custom plugin,
/// otherwise → builtin GTS id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PluginRef {
    /// GTS identifier of a builtin plugin instance.
    BuiltinId(String),
    /// UUID of a custom plugin definition stored in the gear.
    Custom(Uuid),
}

/// A single entry of a plugin chain: a plugin reference plus, optionally,
/// a binding-site configuration override.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PluginItem {
    /// Plain plugin reference (string form on the wire).
    Ref(PluginRef),
    /// Reference plus a per-binding configuration object.
    Configured {
        plugin_ref: PluginRef,
        #[serde(default)]
        config: serde_json::Value,
    },
}

impl PluginItem {
    /// The referenced plugin (builtin id or custom plugin uuid).
    #[must_use]
    pub fn plugin_ref(&self) -> &PluginRef {
        match self {
            Self::Ref(r) | Self::Configured { plugin_ref: r, .. } => r,
        }
    }

    /// Binding-site configuration override, defaulting to `{}`.
    #[must_use]
    pub fn config(&self) -> serde_json::Value {
        match self {
            Self::Ref(_) => serde_json::Value::Object(serde_json::Map::new()),
            Self::Configured { config, .. } => config.clone(),
        }
    }
}

/// Plugin chain block (`plugins` on an upstream or route).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<PluginItem>,
}

/// A custom plugin definition managed through the plugins API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PluginConfig {
    /// The custom plugin's effective configuration. Interpreted by the
    /// builtin implementation selected via `kind` + `name`.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Stored custom plugin resource.
///
/// `tenant_id` is internal bookkeeping and never serializes; `id` does —
/// the management API lists plugins by their server-assigned id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    /// Server-assigned identifier (referenced from binding chains as a
    /// UUID).
    pub id: Uuid,
    /// Owning tenant (hierarchy-scoped visibility).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Kind: auth, guard or transform. Wire field is `type` (matching the
    /// management-API `$filter=type eq 'guard'` surface).
    #[serde(rename = "type")]
    pub kind: PluginKind,
    /// Which builtin implementation this plugin parameterizes.
    pub builtin_type: String,
    /// Human-readable name (unique per tenant).
    pub name: String,
    /// Effective configuration.
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Client-supplied definitions (service input)
// ---------------------------------------------------------------------------

/// Client-supplied upstream definition for create/update.
///
/// `alias` is `None` when the caller wants the service to derive the
/// alias from the endpoints; the service never accepts a conflicting
/// explicit alias for hostname endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamInput {
    pub enabled: bool,
    pub alias: Option<String>,
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: UpstreamProtocol,
    pub auth: Option<AuthConfig>,
    pub headers: Option<HeadersConfig>,
    pub plugins: Option<PluginsConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
}

/// Client-supplied route definition for create/update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInput {
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    pub match_: RouteMatch,
    pub plugins: Option<PluginsConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
}

/// Client-supplied custom plugin definition for create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInput {
    /// Declared kind (`auth` / `guard` / `transform`), matched against
    /// the `builtin_type`'s own kind during validation.
    pub kind: PluginKind,
    /// GTS identifier of the builtin this plugin parameterizes.
    pub builtin_type: String,
    /// Human-readable name (unique per tenant).
    pub name: String,
    /// Effective configuration for the builtin.
    pub config: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Control-plane lifecycle status of an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum UpstreamStatus {
    Enabled,
    Disabled,
}

/// An OAGW upstream service definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Upstream {
    /// Server-assigned identifier (not part of the entity body).
    #[serde(skip)]
    pub id: Uuid,
    /// Owning tenant (not part of the entity body).
    #[serde(skip)]
    pub tenant_id: Uuid,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing alias. Server-derived for hostname endpoints; required
    /// and immutable once set for IP-based endpoints.
    pub alias: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: UpstreamProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

fn default_true() -> bool {
    true
}

/// Header transformation config alias retained for named imports.
pub type HeaderTransformConfig = HeadersConfig;

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Route match block — exactly one of `http` / `grpc` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RouteMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// HTTP inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// An OAGW route: binds inbound traffic to an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Route {
    /// Server-assigned identifier (not part of the entity body).
    #[serde(skip)]
    pub id: Uuid,
    /// Owning tenant (not part of the entity body).
    #[serde(skip)]
    pub tenant_id: Uuid,
    #[serde(default)]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn sample_upstream_json() -> serde_json::Value {
        serde_json::json!({
            "enabled": true,
            "alias": "openai",
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "api.openai.com", "port": 443 }
                ]
            },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "header": "Authorization", "secret_ref": "openai-key" }
            },
            "rate_limit": {
                "sustained": { "rate": 100, "window": "minute" },
                "burst": { "capacity": 200 },
                "scope": "tenant",
                "strategy": "reject",
                "cost": 1
            }
        })
    }

    fn deserialize_sample() -> Upstream {
        serde_json::from_value(sample_upstream_json()).expect("sample upstream parses")
    }

    #[test]
    fn upstream_deserializes_from_wire_shape() {
        let u = deserialize_sample();
        assert_eq!(u.alias, "openai");
        assert_eq!(u.server.endpoints.len(), 1);
        assert_eq!(u.server.endpoints[0].scheme, EndpointScheme::Https);
        assert_eq!(u.protocol, UpstreamProtocol::Http);
        assert!(u.auth.is_some());
        // Defaults from the schema.
        assert_eq!(u.rate_limit.as_ref().unwrap().cost, 1);
        assert_eq!(
            u.rate_limit.as_ref().unwrap().strategy,
            RateLimitStrategy::Reject
        );
    }

    #[test]
    fn upstream_serializes_contract_shape_only() {
        let u = deserialize_sample();
        let v = serde_json::to_value(&u).expect("serialize upstream");
        // Internal fields never surface.
        assert!(v.get("id").is_none());
        assert!(v.get("tenant_id").is_none());
        assert!(v.get("enabled").is_some());
        assert_eq!(v.get("alias").and_then(|x| x.as_str()), Some("openai"));
    }

    #[test]
    fn http_endpoint_allowed_and_defaults_port() {
        let v = sample_upstream_json();
        let u: Upstream =
            serde_json::from_value(v).expect("parse");
        assert!(u.server.endpoints[0].on_default_port());
    }

    #[test]
    fn protocol_gts_ids_round_trip() {
        let v = serde_json::to_value(UpstreamProtocol::Grpc).unwrap();
        assert_eq!(
            v,
            serde_json::json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")
        );
        let p: UpstreamProtocol = serde_json::from_value(v).unwrap();
        assert_eq!(p, UpstreamProtocol::Grpc);
    }

    #[test]
    fn plugin_item_string_and_object_forms() {
        let items: Vec<PluginItem> = serde_json::from_value(serde_json::json!([
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
            { "plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
              "config": { "header_name": "X-Request-Id" } }
        ]))
        .expect("both plugin item forms parse");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].plugin_ref(),
            &PluginRef::BuiltinId(
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1".to_owned()
            )
        );
        assert!(matches!(&items[1], PluginItem::Configured { config, .. } if config["header_name"] == "X-Request-Id"));
    }

    #[test]
    fn route_deserializes_from_wire_shape() {
        let v = serde_json::json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {
                "http": {
                    "methods": ["GET", "POST"],
                    "path": "/v1/chat",
                    "query_allowlist": ["stream"],
                    "path_suffix_mode": "append"
                }
            }
        });
        let r: Route = serde_json::from_value(v).expect("route parses");
        assert_eq!(r.match_.http.as_ref().unwrap().methods.len(), 2);
        assert_eq!(r.match_.http.as_ref().unwrap().path, "/v1/chat");
    }
}
