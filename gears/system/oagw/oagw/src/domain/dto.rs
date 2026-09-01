//! Domain models for the OAGW control plane.
//!
//! These structs mirror the GTS JSON schemas (`schemas/upstream.v1.schema.json`,
//! `schemas/route.v1.schema.json`) and are shared between the REST layer
//! (create/update payloads and responses), the control plane service and the
//! data plane.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::gts_helpers;

/// Sharing mode for hierarchical configuration (PRD §5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; a descendant may override when it specifies its own value.
    Inherit,
    /// Visible; a descendant cannot override.
    Enforce,
}

/// Header passthrough mode for inbound request headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound headers.
    #[default]
    None,
    /// Forward only the headers named in `passthrough_allowlist`.
    Allowlist,
    /// Forward all inbound headers (minus hop-by-hop stripping).
    All,
}

/// WebSocket / HTTP endpoint scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    #[default]
    Https,
    #[allow(clippy::upper_case_acronyms)]
    Wss,
    #[allow(clippy::upper_case_acronyms)]
    Wt,
    Grpc,
    Http,
}

impl EndpointScheme {
    /// Whether the scheme is plain-`http` (subject to `allow_http_upstream`).
    #[must_use]
    pub fn is_http(self) -> bool {
        matches!(self, Self::Http)
    }

    /// The standard default port for this scheme.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

/// A single upstream server endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    #[serde(default)]
    pub scheme: EndpointScheme,
    pub host: String,
    #[serde(default = "Endpoint::default_port_field")]
    pub port: u16,
}

impl Endpoint {
    #[must_use]
    pub fn default_port_field() -> u16 {
        443
    }

    /// The host as it appears in an `X-OAGW-Target-Host` value / alias.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        self.host.trim_end_matches('.').to_ascii_lowercase()
    }
}

/// Upstream server configuration (one or more endpoints forming a pool).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// Header transformation rules (upstream.v1 schema `headers`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderRules {
    /// Request-side transforms (applied to the outbound request).
    pub request: HeaderTransform,
    /// Response-side transforms (applied to the response to the client).
    pub response: HeaderTransform,
}

/// One side of the header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderTransform {
    /// Headers to overwrite.
    pub set: HashMap<String, String>,
    /// Headers to append (allowed to duplicate).
    pub add: HashMap<String, String>,
    /// Header names to remove.
    pub remove: Vec<String>,
    /// Inbound passthrough policy (request side only).
    pub passthrough: PassthroughMode,
    /// Allowlist used when `passthrough == allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

impl Default for HeaderTransform {
    fn default() -> Self {
        Self {
            set: HashMap::new(),
            add: HashMap::new(),
            remove: Vec::new(),
            passthrough: PassthroughMode::None,
            passthrough_allowlist: Vec::new(),
        }
    }
}

/// Sustained rate specification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateSpec {
    /// Tokens replenished per window.
    pub rate: u64,
    #[serde(default)]
    pub window: RateWindow,
}

/// Sustained window unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    /// Window length in seconds.
    #[must_use]
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Burst capacity specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstSpec {
    pub capacity: u64,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Rate limit enforcement strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429 + `Retry-After`.
    #[default]
    Reject,
    /// Queue for later execution within bounded capacity.
    Queue,
    /// Process with reduced functionality.
    Degrade,
}

/// Rate limiting configuration (ADR-0003 dual-rate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    pub sustained: RateSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstSpec>,
    #[serde(default)]
    pub scope: RateScope,
    #[serde(default)]
    pub strategy: RateStrategy,
    #[serde(default = "RateLimitConfig::default_cost")]
    pub cost: u64,
    #[serde(default = "RateLimitConfig::default_response_headers")]
    pub response_headers: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: RateSpec::default(),
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: Self::default_cost(),
            response_headers: Self::default_response_headers(),
        }
    }
}

impl RateLimitConfig {
    fn default_cost() -> u64 {
        1
    }
    fn default_response_headers() -> bool {
        true
    }

    /// Effective burst capacity (`burst.capacity` or `sustained.rate`).
    #[must_use]
    pub fn burst_capacity(&self) -> u64 {
        self.burst.map_or(self.sustained.rate, |b| b.capacity)
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "CorsConfig::default_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: Self::default_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl CorsConfig {
    fn default_methods() -> Vec<String> {
        vec!["GET".to_owned(), "POST".to_owned()]
    }

    /// Whether the given origin is allowed (exact match or wildcard).
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    /// Whether the given method is allowed.
    #[must_use]
    pub fn method_allowed(&self, method: &str) -> bool {
        self.allowed_methods.iter().any(|m| m == method)
    }

    /// Validate the config. Returns a description of the first problem.
    #[must_use]
    pub fn validation_error(&self) -> Option<String> {
        if self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*") {
            return Some("cannot use allow_credentials with wildcard origin".to_owned());
        }
        None
    }
}

/// One plugin binding on an upstream or route.
///
/// Accepts either a bare GTS identifier string or `{ "plugin_ref": ..., "config": {...} }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginBinding {
    /// Full GTS identifier of the plugin.
    pub plugin_ref: String,
    /// Plugin configuration (object).
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub config: serde_json::Map<String, serde_json::Value>,
}

impl PluginBinding {
    #[must_use]
    pub fn new(plugin_ref: impl Into<String>) -> Self {
        Self {
            plugin_ref: plugin_ref.into(),
            config: serde_json::Map::new(),
        }
    }

    /// The uuid part of the plugin ref when it is uuid-backed.
    #[must_use]
    pub fn plugin_uuid(&self) -> Option<Uuid> {
        gts_helpers::plugin_uuid_from_id(&self.plugin_ref)
    }
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Obj {
                #[serde(rename = "plugin_ref")]
                plugin_ref: String,
                #[serde(default)]
                config: serde_json::Map<String, serde_json::Value>,
            },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Str(plugin_ref) => Ok(PluginBinding {
                plugin_ref,
                config: serde_json::Map::new(),
            }),
            Raw::Obj { plugin_ref, config } => Ok(PluginBinding { plugin_ref, config }),
        }
    }
}

/// Ordered plugin chain configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    pub items: Vec<PluginBinding>,
}

/// Auth plugin configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// GTS plugin identifier for the auth plugin.
    #[serde(rename = "type")]
    pub plugin_type: Option<String>,
    pub sharing: SharingMode,
    pub config: serde_json::Map<String, serde_json::Value>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            plugin_type: None,
            sharing: SharingMode::Private,
            config: serde_json::Map::new(),
        }
    }
}

/// An upstream configuration resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstream {
    /// Server-assigned id. `None` on create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. `None` on create; filled at persist time from the
    /// security context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Whether the upstream is enabled.
    pub enabled: bool,
    /// Routing alias (auto-derived or explicit). `None` before derivation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default = "Upstream::default_protocol")]
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderRules>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    fn default_protocol() -> String {
        gts_helpers::HTTP_PROTOCOL_ID.to_owned()
    }

    /// `self.server.endpoints.first()` host (used for single-endpoint cases).
    #[must_use]
    pub fn primary_endpoint(&self) -> Option<&Endpoint> {
        self.server.endpoints.first()
    }

    /// Whether this upstream's protocol is gRPC.
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.protocol == gts_helpers::GRPC_PROTOCOL_ID
    }

    /// The effective (possibly merged) auth config or a default private one.
    #[must_use]
    pub fn effective_auth(&self) -> Option<&AuthConfig> {
        self.auth.as_ref()
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: None,
            tenant_id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: gts_helpers::HTTP_PROTOCOL_ID.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

/// HTTP match rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMatch {
    /// Allowed HTTP methods (e.g. `GET`, `POST`).
    pub methods: Vec<String>,
    /// Path pattern.
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How the proxy URL path suffix is joined to the route path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests that carry a path suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// gRPC match rules (catalog-only today; no gRPC proxy code path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Route match: exactly one of `http` or `grpc` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MatchConfig {
    Http { http: HttpMatch },
    Grpc { grpc: GrpcMatch },
}

impl MatchConfig {
    /// The HTTP match if this is an HTTP route.
    #[must_use]
    pub fn as_http(&self) -> Option<&HttpMatch> {
        match self {
            Self::Http { http } => Some(http),
            Self::Grpc { .. } => None,
        }
    }
}

/// A route configuration resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    /// Server-assigned id. `None` on create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. `None` on create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Referenced upstream id (immutable after creation).
    pub upstream_id: Uuid,
    pub r#match: MatchConfig,
    /// Whether this route may match requests. Disabled routes are excluded.
    #[serde(default = "Route::default_enabled")]
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl Route {
    fn default_enabled() -> bool {
        true
    }
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: None,
            tenant_id: None,
            tags: Vec::new(),
            upstream_id: Uuid::nil(),
            r#match: MatchConfig::Http {
                http: HttpMatch {
                    methods: Vec::new(),
                    path: String::new(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                },
            },
            enabled: true,
            plugins: None,
            rate_limit: None,
        }
    }
}

/// A custom (uuid-backed) plugin resource. Named plugins are not persisted;
/// they live in the in-process registries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Plugin {
    /// Server-assigned id. `None` on create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Owning tenant. `None` on create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    /// Plugin family: `auth`, `guard` or `transform`.
    #[allow(clippy::struct_field_names)]
    // field name is the serialized wire name; renaming would change the API
    pub plugin_type: String,
    /// Human-readable name.
    pub name: String,
    /// Short description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema describing allowed config.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Starlark source code (for custom plugins).
    #[serde(default)]
    pub source_code: String,
    /// Declared phases (transform plugins): e.g. `["on_request", "on_response"]`.
    #[serde(default)]
    pub phases: Vec<String>,
}

impl Plugin {
    /// The anonymous GTS identifier for this plugin.
    #[must_use]
    pub fn gts_id(&self) -> String {
        let prefix = match self.plugin_type.as_str() {
            "auth" => gts_helpers::AUTH_PLUGIN_TYPE_ID,
            "guard" => gts_helpers::GUARD_PLUGIN_TYPE_ID,
            _ => gts_helpers::TRANSFORM_PLUGIN_TYPE_ID,
        };
        let uuid = self.id.unwrap_or_default();
        format!("{prefix}{uuid}")
    }
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: None,
            tenant_id: None,
            plugin_type: "transform".to_owned(),
            name: String::new(),
            description: None,
            config_schema: serde_json::Value::Object(serde_json::Map::new()),
            source_code: String::new(),
            phases: Vec::new(),
        }
    }
}

/// The resolved (effective) view of an upstream that the data plane
/// consumes. Field-level merges across the tenant chain are applied by the
/// control plane before this is handed to the proxy orchestrator.
#[derive(Debug, Clone)]
pub struct EffectiveUpstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub alias: String,
    pub enabled: bool,
    pub protocol: String,
    pub server: ServerConfig,
    pub auth: Option<AuthConfig>,
    pub headers: HeaderRules,
    pub plugins: PluginsConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    /// Whether this upstream's alias was derived from a common multi-host
    /// suffix (which makes `X-OAGW-Target-Host` mandatory).
    pub alias_is_common_suffix: bool,
}

impl EffectiveUpstream {
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.protocol == gts_helpers::GRPC_PROTOCOL_ID
    }
}

/// The resolved (effective) view of a route for the data plane.
#[derive(Debug, Clone)]
pub struct EffectiveRoute {
    pub id: Uuid,
    pub upstream_id: Uuid,
    pub r#match: MatchConfig,
    pub enabled: bool,
    pub plugins: PluginsConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plugin_binding_deserializes_string_and_object() {
        let s: PluginBinding = serde_json::from_str(r#""gts...~cf.core.oagw.noop.v1""#).unwrap();
        assert_eq!(s.plugin_ref, "gts...~cf.core.oagw.noop.v1");
        assert!(s.config.is_empty());

        let o: PluginBinding = serde_json::from_str(
            r#"{"plugin_ref":"gts...~cf.core.oagw.apikey.v1","config":{"header":"X-Api-Key"}}"#,
        )
        .unwrap();
        assert_eq!(o.config.get("header").unwrap(), "X-Api-Key");
    }

    #[test]
    fn burst_capacity_defaults_to_sustained_rate() {
        let cfg: RateLimitConfig = serde_json::from_value(serde_json::json!({
            "sustained": { "rate": 100, "window": "minute" }
        }))
        .unwrap();
        assert_eq!(cfg.burst_capacity(), 100);
        assert!(cfg.response_headers);
        assert_eq!(cfg.strategy, RateStrategy::Reject);
    }

    #[test]
    fn cors_credentials_with_wildcard_is_invalid() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".into()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert!(cfg.validation_error().is_some());
    }

    #[test]
    fn upstream_defaults_to_http_protocol() {
        let u: Upstream = serde_json::from_value(serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com", "port": 443 }] }
        }))
        .unwrap();
        assert!(u.enabled);
        assert_eq!(u.protocol, gts_helpers::HTTP_PROTOCOL_ID);
        assert_eq!(u.primary_endpoint().unwrap().host, "api.example.com");
    }
}
