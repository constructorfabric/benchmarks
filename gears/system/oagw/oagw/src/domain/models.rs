//! Entity and configuration models for the OAGW gear.
//!
//! Field shapes, defaults, enums, and constraints mirror
//! `gears/system/oagw/docs/schemas/upstream.v1.schema.json` and
//! `route.v1.schema.json` exactly. `id` / `tenant_id` are server-managed and
//! optional on the wire so a client-created body may omit them; the service
//! fills them at creation time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::gts;

// ---------------------------------------------------------------------------
// Small enums
// ---------------------------------------------------------------------------

/// Hierarchical config sharing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    #[default]
    Private,
    Inherit,
    Enforce,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Rate limit time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateLimitWindow {
    /// Number of tokens replenished *per second* for a given `rate`, so that
    /// `rate` means "tokens per window".
    #[must_use]
    pub fn refill_per_second(&self, rate: u32) -> f64 {
        let rate = f64::from(rate);
        match self {
            Self::Second => rate,
            Self::Minute => rate / 60.0,
            Self::Hour => rate / 3600.0,
            Self::Day => rate / 86_400.0,
        }
    }
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Rate limit strategy when capacity is exceeded. Only `reject` is
/// implemented by the data plane; `queue`/`degrade` degrade to `reject` with
/// a warning at validation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Path suffix behavior for `/proxy/{alias}/{*path}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    Disabled,
    #[default]
    Append,
}

/// Inbound header passthrough mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    #[default]
    None,
    Allowlist,
    All,
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_one() -> u32 {
    1
}

fn default_port() -> u16 {
    443
}

fn default_scheme() -> String {
    "https".to_owned()
}

fn default_window() -> RateLimitWindow {
    RateLimitWindow::Second
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// An outbound upstream service registered by a tenant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Upstream {
    /// System-generated unique identifier. Server-managed (read-only on wire).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. Server-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Whether requests to this upstream are allowed. When a parent tenant
    /// disables an upstream, it is disabled for all descendants.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Human-readable routing identifier (also the `/proxy/{alias}` key).
    pub alias: String,
    /// Flat tags, `^[a-z0-9_-]+$`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint set.
    pub server: ServerConfig,
    /// Upstream protocol as GTS identifier.
    pub protocol: String,
    /// Upstream authentication.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeaderTransforms,
    /// Plugin bindings.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    /// Human-kind label used by canonical `NotFound` errors.
    pub const KIND: &'static str = "upstream";

    /// Effective endpoint host set (normalized lowercase).
    #[must_use]
    pub fn endpoint_hosts(&self) -> Vec<String> {
        self.server
            .endpoints
            .iter()
            .map(|e| e.normalized_host())
            .collect()
    }
}

/// Server configuration for an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    /// One or more endpoints (`minItems: 1`).
    pub endpoints: Vec<Endpoint>,
}

/// A single upstream endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// `https` (default), `wss`, `wt`, `grpc`; `http` accepted only when the
    /// gear is configured with `allow_http_upstream`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname or IP address.
    pub host: String,
    /// Service port, 1..=65535 (default 443).
    #[serde(default = "default_port")]
    pub port: u16,
}

impl Endpoint {
    /// Whether this endpoint uses the scheme's standard port.
    #[must_use]
    pub fn is_standard_port(&self) -> bool {
        match self.scheme.as_str() {
            "http" => self.port == 80,
            "https" | "wss" | "wt" | "grpc" => self.port == 443,
            _ => false,
        }
    }

    /// Host with trailing dot stripped and lowercased.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        self.host.trim_end_matches('.').to_ascii_lowercase()
    }

    /// `host` or `host:port` (non-standard port only).
    #[must_use]
    pub fn authority(&self) -> String {
        if self.is_standard_port() {
            self.normalized_host()
        } else {
            format!("{}:{}", self.normalized_host(), self.port)
        }
    }

    /// `scheme://authority`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("{}://{}", self.scheme, self.authority())
    }
}

/// Authentication configuration for an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    /// Auth plugin type (GTS identifier of an `auth_plugin`).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Sharing mode for hierarchical config.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Auth plugin configuration (free-form).
    #[serde(default, skip_serializing_if = "is_empty_object")]
    pub config: Value,
}

fn is_empty_object(v: &Value) -> bool {
    v.is_null() || (v.as_object().is_some_and(|o| o.is_empty()))
}

// ---------------------------------------------------------------------------
// Header transforms
// ---------------------------------------------------------------------------

/// Header transformation rules for an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeaderTransforms {
    #[serde(default, skip_serializing_if = "is_default_request_header_rules")]
    pub request: RequestHeaderRules,
    #[serde(default, skip_serializing_if = "is_default_response_header_rules")]
    pub response: ResponseHeaderRules,
}

/// Inbound request header rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if exists) on the outbound request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, allow duplicates).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward to the upstream.
    #[serde(default, skip_serializing_if = "is_default_passthrough")]
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough == allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Upstream response header rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResponseHeaderRules {
    /// Headers to set on the client-facing response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add on the client-facing response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

fn is_default_passthrough(m: &PassthroughMode) -> bool {
    *m == PassthroughMode::None
}

fn is_default_request_header_rules(r: &RequestHeaderRules) -> bool {
    r == &RequestHeaderRules::default()
}

fn is_default_response_header_rules(r: &ResponseHeaderRules) -> bool {
    r == &ResponseHeaderRules::default()
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Plugin binding configuration for an upstream or route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginsConfig {
    /// Sharing mode for the plugin chain (upstreams only).
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered plugin list. Items are either a plain string (builtin GTS
    /// identifier or custom plugin UUID) or an object binding
    /// `{plugin_ref, config}` (ADR 0009).
    #[serde(default)]
    pub items: Vec<PluginItem>,
}

/// A single plugin reference within `plugins.items`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PluginItem {
    /// Plain `"gts...~..."` builtin identifier or `"uuid"` custom reference.
    Ref(String),
    /// Explicit binding with instance-level config (`{plugin_ref, config}`).
    Binding(PluginBinding),
}

impl PluginItem {
    /// The referenced plugin identifier (GTS id or UUID string).
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Ref(id) => id,
            Self::Binding(b) => b.plugin_ref.as_str(),
        }
    }

    /// Instance-level config for this binding.
    #[must_use]
    pub fn config(&self) -> Value {
        match self {
            Self::Ref(_) => Value::Object(Default::default()),
            Self::Binding(b) => b.config.clone(),
        }
    }
}

/// Explicit plugin binding with instance-level config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginBinding {
    pub plugin_ref: String,
    #[serde(default)]
    pub config: Value,
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Rate limiting configuration (upstream or route scoped).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical composition.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm (only `token_bucket` is implemented).
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained refill policy.
    pub sustained: SustainedRate,
    /// Burst capacity (defaults to `sustained.rate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// Excess behavior (`reject` implemented; others degrade to reject).
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request (weighted endpoints).
    #[serde(default = "default_one")]
    pub cost: u32,
}

/// Sustained refill policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Time window.
    #[serde(default = "default_window")]
    pub window: RateLimitWindow,
}

/// Burst configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BurstConfig {
    /// Maximum burst size (bucket capacity), defaults to `sustained.rate`.
    #[serde(default = "default_one")]
    pub capacity: u32,
}

impl RateLimitConfig {
    /// Effective bucket capacity: explicit burst, else the sustained rate.
    #[must_use]
    pub fn bucket_capacity(&self) -> u32 {
        self.burst
            .as_ref()
            .map(|b| b.capacity)
            .unwrap_or(self.sustained.rate)
            .max(1)
    }
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// CORS configuration for an upstream or route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct CorsConfig {
    /// Sharing mode for hierarchical composition.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS enforcement for this upstream/route.
    #[serde(default = "default_false")]
    pub enabled: bool,
    /// Allowed origins (`["*"]` for any; not allowed with credentials).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods (default `["GET", "POST"]`).
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Allow credentials (requires specific origins, not `*`).
    #[serde(default = "default_false")]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Whether `allow_credentials` is combined with a wildcard origin — an
    /// invalid combination per the schema (`then` constraint).
    #[must_use]
    pub fn has_invalid_wildcard_with_credentials(&self) -> bool {
        self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*")
    }

    /// Whether `origin` is allowed by this config (exact match or wildcard).
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    /// Whether `method` is allowed by this config.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        method.eq_ignore_ascii_case("OPTIONS")
            || self
                .allowed_methods
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method))
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// A route binding a match rule to an upstream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    /// System-generated unique identifier. Server-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. Server-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Target upstream (belongs to the same tenant).
    pub upstream_id: Uuid,
    /// Route is active. Present on routes by mandate of the PRD/DESIGN even
    /// though the JSON schema omits it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match rules (exactly one of `http` | `grpc`).
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Flat tags, `^[a-z0-9_-]+$`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Route-level plugin bindings.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Route-level rate limiting (overrides upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS (overrides upstream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Route {
    /// Human-kind label used by canonical `NotFound` errors.
    pub const KIND: &'static str = "route";
}

/// Route match: either HTTP rules or gRPC rules, exactly one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum RouteMatch {
    Http(HttpMatch),
    Grpc(GrpcMatch),
}

impl RouteMatch {
    /// The discriminator value (`"http"` | `"grpc"`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Http(_) => "http",
            Self::Grpc(_) => "grpc",
        }
    }

    #[must_use]
    pub fn as_http(&self) -> Option<&HttpMatch> {
        match self {
            Self::Http(m) => Some(m),
            Self::Grpc(_) => None,
        }
    }
}

/// HTTP match rules.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpMatch {
    /// Allowed methods (`GET`/`POST`/`PUT`/`DELETE`/`PATCH`), min 1.
    pub methods: Vec<String>,
    /// Path prefix, min length 1.
    pub path: String,
    /// Whitelisted query parameters (empty = allow none).
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Path suffix handling for `/proxy/{alias}/{*path}`.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (catalogued; no gRPC data-plane path is implemented).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

// ---------------------------------------------------------------------------
// Plugin (custom Starlark)
// ---------------------------------------------------------------------------

/// A custom tenant-defined plugin definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Plugin {
    /// System-generated unique identifier. Server-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. Server-managed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Plugin type (GTS identifier of the plugin category being customized).
    pub plugin_type: String,
    /// Display name.
    pub name: String,
    /// JSON schema describing the plugin config surface.
    #[serde(default, skip_serializing_if = "is_empty_object")]
    pub config_schema: Value,
    /// Starlark source code.
    pub source_code: String,
}

impl Plugin {
    /// Human-kind label used by canonical `NotFound` errors.
    pub const KIND: &'static str = "plugin";
}

// ---------------------------------------------------------------------------
// Well-known protocol / auth GTS shorthands
// ---------------------------------------------------------------------------

/// Whether `id` is one of the two known upstream protocol identifiers.
#[must_use]
pub fn is_supported_protocol(id: &str) -> bool {
    id == gts::PROTOCOL_HTTP || id == gts::PROTOCOL_GRPC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_authority_standard_port() {
        let e = Endpoint {
            scheme: "https".into(),
            host: "api.vendor.com".into(),
            port: 443,
        };
        assert_eq!(e.authority(), "api.vendor.com");
        assert_eq!(e.base_url(), "https://api.vendor.com");
    }

    #[test]
    fn endpoint_authority_nonstandard_port() {
        let e = Endpoint {
            scheme: "http".into(),
            host: "localhost".into(),
            port: 8080,
        };
        assert_eq!(e.authority(), "localhost:8080");
        assert_eq!(e.base_url(), "http://localhost:8080");
    }

    #[test]
    fn endpoint_normalizes_host() {
        let e = Endpoint {
            scheme: "https".into(),
            host: "API.Vendor.COM.".into(),
            port: 443,
        };
        assert_eq!(e.normalized_host(), "api.vendor.com");
    }

    #[test]
    fn rate_limit_bucket_capacity_defaults_to_rate() {
        let rl = RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 5,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        };
        assert_eq!(rl.bucket_capacity(), 5);
        let with_burst = RateLimitConfig {
            burst: Some(BurstConfig { capacity: 10 }),
            ..rl.clone()
        };
        assert_eq!(with_burst.bucket_capacity(), 10);
    }

    #[test]
    fn cors_wildcard_with_credentials_is_invalid() {
        let cors = CorsConfig {
            allow_credentials: true,
            allowed_origins: vec!["*".into()],
            ..Default::default()
        };
        assert!(cors.has_invalid_wildcard_with_credentials());
        let cors = CorsConfig {
            allow_credentials: true,
            allowed_origins: vec!["https://app.example.com".into()],
            ..Default::default()
        };
        assert!(!cors.has_invalid_wildcard_with_credentials());
    }

    #[test]
    fn untagged_plugin_item_deserializes_both_forms() {
        let a: PluginItem = serde_json::from_str(r#""gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1""#)
            .unwrap();
        assert!(matches!(a, PluginItem::Ref(_)));
        let b: PluginItem = serde_json::from_str(
            r#"{"pluginRef":"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1","config":{"required_request_headers":"x-correlation-id"}}"#,
        )
        .unwrap();
        assert!(matches!(b, PluginItem::Binding(_)));
        assert_eq!(b.plugin_ref(), "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
        assert!(b.config().get("required_request_headers").is_some());
    }

    #[test]
    fn route_match_deserializes_http() {
        let r: RouteMatch = serde_json::from_str(
            r#"{"methods":["GET"],"path":"/v1/chat"}"#,
        )
        .unwrap();
        assert_eq!(r.kind(), "http");
        let http = r.as_http().unwrap();
        assert_eq!(http.path, "/v1/chat");
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
    }
}
