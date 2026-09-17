//! OAGW domain model — upstream, route and plugin configuration records.
//!
//! Shapes follow `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`, extended with the `http` endpoint
//! scheme that the gateway accepts when `allow_http_upstream` is enabled.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Upstream endpoint scheme.
///
/// `http` is only usable when the gear-level `allow_http_upstream` flag is on;
/// `https`/`wss`/`wt`/`grpc` always imply TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP (requires `allow_http_upstream`).
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport (treated as TLS).
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl EndpointScheme {
    /// `true` when the scheme implies a TLS connection.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }

    /// Default port for the scheme, used when none is configured.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

/// Default port used when an endpoint omits `port` (schema default: 443).
const fn schema_default_port() -> u16 {
    443
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub scheme: EndpointScheme,
    pub host: String,
    #[serde(default = "schema_default_port")]
    pub port: u16,
}

/// Upstream connection targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamServer {
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical configuration sharing mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may inherit and override.
    Inherit,
    /// Descendants inherit and cannot override.
    Enforce,
}

/// Outbound authentication plugin binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: String,
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin configuration (`ctx.config` keys).
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Header passthrough mode for inbound requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPassthrough {
    /// Forward nothing (only `set`/`add` rules apply).
    #[default]
    None,
    /// Forward only `passthrough_allowlist` entries.
    Allowlist,
    /// Forward everything except hop-by-hop headers.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestHeaderRules {
    /// Overwrite if present.
    pub set: BTreeMap<String, String>,
    /// Append (duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Strip from the inbound request.
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded upstream.
    pub passthrough: HeaderPassthrough,
    /// Headers forwarded when `passthrough == allowlist`.
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResponseHeaderRules {
    /// Overwrite if present.
    pub set: BTreeMap<String, String>,
    /// Append (duplicates allowed).
    pub add: BTreeMap<String, String>,
    /// Strip from the upstream response.
    pub remove: Vec<String>,
}

/// Header transformation configuration (DESIGN "Headers Transformation").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadersConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// One entry of a plugin chain: a bare plugin identifier, or the same
/// identifier carrying its per-binding configuration (ADR-0009 "Upstream
/// Configuration Example").
///
/// The two wire shapes are accepted because the ADR example binds
/// `required_headers.v1` as `{"plugin_ref": ..., "config": {...}}` while the
/// schema also permits a bare `gts-identifier`/`uuid` string. A binding
/// without configuration serialises back as the bare string, so round-trips
/// stay faithful to what the caller sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBinding {
    /// Builtin plugin GTS identifier (or custom plugin UUID) with no
    /// per-binding configuration.
    Ref(String),
    /// `{"plugin_ref": <gts identifier>, "config": <object>}`.
    Bound {
        /// Canonical plugin identifier.
        plugin_ref: String,
        /// Configuration handed to the plugin for this binding.
        #[serde(default)]
        config: serde_json::Value,
    },
}

impl PluginBinding {
    /// The bound plugin identifier, in either shape.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Ref(id) => id,
            Self::Bound { plugin_ref, .. } => plugin_ref,
        }
    }

    /// `true` when this binding names `plugin_id`.
    ///
    /// A bare custom-plugin UUID and its full GTS id denote the same plugin,
    /// so both spellings match.
    #[must_use]
    pub fn matches(&self, plugin_id: &str) -> bool {
        let id = self.id();
        if id == plugin_id {
            return true;
        }
        match (
            crate::domain::gts_helpers::resource_uuid(id),
            crate::domain::gts_helpers::resource_uuid(plugin_id),
        ) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Per-binding configuration; [`serde_json::Value::Null`] when absent.
    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        match self {
            Self::Ref(_) | Self::Bound { config: serde_json::Value::Null, .. } => {
                &serde_json::Value::Null
            }
            Self::Bound { config, .. } => config,
        }
    }
}

#[cfg(test)]
mod plugin_binding_tests {
    use super::*;

    #[test]
    fn bare_identifier_round_trips() {
        let raw = r#"["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"]"#;
        let items: Vec<PluginBinding> = serde_json::from_str(raw).unwrap();
        assert_eq!(items[0].id(), "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
        assert_eq!(*items[0].config(), serde_json::Value::Null);
        // Without configuration the binding serialises back as a bare string.
        assert_eq!(serde_json::to_string(&items).unwrap(), raw);
    }

    #[test]
    fn adr_0009_binding_shape_carries_config() {
        let raw = r#"[{"plugin_ref":"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1","config":{"required_request_headers":"x-correlation-id,accept","required_response_headers":"content-type"}}]"#;
        let items: Vec<PluginBinding> = serde_json::from_str(raw).unwrap();
        assert_eq!(
            items[0].id(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert_eq!(
            items[0].config()["required_request_headers"],
            "x-correlation-id,accept"
        );
        // Configured bindings keep their ADR-0009 shape on the way out.
        assert_eq!(serde_json::to_string(&items).unwrap(), raw);
    }

    #[test]
    fn bound_without_config_is_null_not_object() {
        let binding: PluginBinding =
            serde_json::from_str(r#"{"plugin_ref":"a.v1~x"}"#).unwrap();
        assert_eq!(*binding.config(), serde_json::Value::Null);
    }
}

/// Plugin chain configuration: built-in GTS ids or custom plugin UUIDs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginListConfig {
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin bindings, upstream-relative order preserved.
    pub items: Vec<PluginBinding>,
}

/// Sustained rate window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    pub fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Sustained rate: `rate` tokens per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    pub rate: u64,
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst bucket capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstCapacity {
    pub capacity: u64,
}

/// Token-bucket rate limiting configuration (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    pub sharing: Sharing,
    pub algorithm: RateAlgorithm,
    pub sustained: SustainedRate,
    /// Defaults to `sustained.rate` when absent.
    pub burst: Option<BurstCapacity>,
    pub scope: RateScope,
    pub strategy: RateStrategy,
    pub cost: u64,
    /// Emit `X-RateLimit-*` headers on responses.
    pub response_headers: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        }
    }
}

impl RateLimitConfig {
    /// Effective bucket capacity: `burst.capacity` or `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst.map_or(self.sustained.rate, |b| b.capacity)
    }

    /// Refill rate in tokens per second.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.secs() as f64
    }
}

/// Per-upstream/route CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    pub sharing: Sharing,
    pub enabled: bool,
    /// Origins; `["*"]` allows any origin.
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// HTTP methods routable by OAGW.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
    Patch,
}

impl HttpMethod {
    /// Parses an HTTP method token, returning `None` for unsupported methods.
    #[must_use]
    pub fn parse(method: &http::Method) -> Option<Self> {
        match *method {
            http::Method::GET => Some(Self::Get),
            http::Method::POST => Some(Self::Post),
            http::Method::PUT => Some(Self::Put),
            http::Method::DELETE => Some(Self::Delete),
            http::Method::PATCH => Some(Self::Patch),
            _ => None,
        }
    }

    /// `http::Method` for this variant.
    #[must_use]
    pub fn as_method(self) -> &'static http::Method {
        match self {
            Self::Get => &http::Method::GET,
            Self::Post => &http::Method::POST,
            Self::Put => &http::Method::PUT,
            Self::Delete => &http::Method::DELETE,
            Self::Patch => &http::Method::PATCH,
        }
    }
}

/// How the `{path_suffix}` segment of the proxy URL is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Requests carrying a path suffix are rejected.
    Disabled,
    /// The suffix is appended to `match.http.path`.
    #[default]
    Append,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,
    pub path: String,
    /// Allowed query parameters; empty allows none.
    pub query_allowlist: Vec<String>,
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: vec![HttpMethod::Get],
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// gRPC match rules (catalogued; no proxy path yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Protocol-scoped inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchRule {
    /// HTTP match rules, if this route matches on HTTP.
    #[must_use]
    pub fn http(&self) -> Option<&HttpMatch> {
        self.http.as_ref()
    }

    /// `true` when this route matches on the gRPC key set.
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.grpc.is_some()
    }
}

/// Route match key used for conflict detection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MatchKey {
    Http {
        method: HttpMethod,
        path: String,
    },
    Grpc {
        service: String,
        method: String,
    },
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// Creation/replace payload for an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamInput {
    pub enabled: bool,
    /// Explicit alias. Absent means "derive from the endpoints".
    pub alias: Option<String>,
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    /// Protocol GTS identifier.
    pub protocol: String,
    pub auth: Option<AuthConfig>,
    pub headers: Option<HeadersConfig>,
    pub plugins: Option<PluginListConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
}

impl Default for UpstreamInput {
    fn default() -> Self {
        Self {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: UpstreamServer {
                endpoints: Vec::new(),
            },
            protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

/// Stored upstream configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub enabled: bool,
    /// Resolved alias (derived or explicit), ASCII-lowercase.
    pub alias: String,
    /// `true` when `alias` was supplied by the caller rather than derived.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alias_explicit: bool,
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginListConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Epoch milliseconds.
    pub created_at: u64,
    /// Epoch milliseconds.
    pub updated_at: u64,
}

/// Creation payload for a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteInput {
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    pub r#match: MatchRule,
    pub plugins: PluginListConfig,
    pub rate_limit: Option<RateLimitConfig>,
}

impl Default for RouteInput {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            upstream_id: Uuid::nil(),
            r#match: MatchRule {
                http: None,
                grpc: None,
            },
            plugins: PluginListConfig::default(),
            rate_limit: None,
        }
    }
}

/// Stored route configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    pub r#match: MatchRule,
    #[serde(default)]
    pub plugins: PluginListConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Epoch milliseconds.
    pub created_at: u64,
    /// Epoch milliseconds.
    pub updated_at: u64,
}

impl Route {
    /// Match key used for conflict detection and request matching.
    #[must_use]
    pub fn match_keys(&self) -> Vec<MatchKey> {
        match &self.r#match {
            MatchRule {
                http: Some(http), ..
            } => http
                .methods
                .iter()
                .map(|m| MatchKey::Http {
                    method: *m,
                    path: http.path.clone(),
                })
                .collect(),
            MatchRule {
                grpc: Some(grpc), ..
            } => vec![MatchKey::Grpc {
                service: grpc.service.clone(),
                method: grpc.method.clone(),
            }],
            _ => Vec::new(),
        }
    }
}

/// Plugin kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    Auth,
    Guard,
    Transform,
}

impl PluginType {
    /// GTS namespace prefix for this plugin kind.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Auth => crate::domain::gts_helpers::AUTH_PLUGIN_PREFIX,
            Self::Guard => crate::domain::gts_helpers::GUARD_PLUGIN_PREFIX,
            Self::Transform => crate::domain::gts_helpers::TRANSFORM_PLUGIN_PREFIX,
        }
    }
}

/// Creation payload for a custom plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub plugin_type: PluginType,
    #[serde(default)]
    pub config_schema: serde_json::Value,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Starlark source for custom plugins.
    #[serde(default)]
    pub source_code: String,
}

impl Default for PluginInput {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            plugin_type: PluginType::Transform,
            config_schema: serde_json::Value::Null,
            config: serde_json::Value::Null,
            source_code: String::new(),
        }
    }
}

/// Stored plugin configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}` for custom plugins, or the
    /// builtin GTS identifier for catalogued plugins.
    pub id: String,
    pub tenant_id: Uuid,
    pub name: String,
    pub description: String,
    pub plugin_type: PluginType,
    #[serde(default)]
    pub config_schema: serde_json::Value,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(default)]
    pub source_code: String,
    /// Epoch milliseconds.
    pub created_at: u64,
    /// Epoch milliseconds.
    pub updated_at: u64,
}
