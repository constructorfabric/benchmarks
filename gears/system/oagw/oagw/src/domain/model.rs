//! Domain entities and their configuration value objects.
//!
//! The serde shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` so a persisted entity and its REST
//! projection stay the same document.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::domain::gts_helpers;

// ---------------------------------------------------------------------------
// Shared enums
// ---------------------------------------------------------------------------

/// Hierarchical visibility of a configuration block (`cpt-cf-oagw-fr-hierarchical-config`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    #[default]
    Private,
    Inherit,
    Enforce,
}

/// Endpoint scheme.
///
/// `DESIGN.md`'s `cpt-cf-oagw-constraint-https-only` describes the default
/// *connection* posture; which schemes the field accepts is a separate
/// question, so the plaintext family is part of the enum and is gated at
/// connect time by `OagwConfig::allow_http_upstream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Https,
    Http,
    Wss,
    Ws,
    Wt,
    Grpc,
}

impl Scheme {
    /// Port omitted from a derived alias (HTTP/WS: 80, everything else: 443).
    #[must_use]
    pub fn standard_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether a connection with this scheme is wrapped in TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Wt | Self::Grpc)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Http => "http",
            Self::Wss => "wss",
            Self::Ws => "ws",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// Which inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    #[default]
    None,
    Allowlist,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateLimitWindow {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BudgetMode {
    #[default]
    Unlimited,
    Allocated,
    Shared,
}

/// How `/{path_suffix}` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    Disabled,
    #[default]
    Append,
}

// ---------------------------------------------------------------------------
// Server / endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
}

/// Wire form of an endpoint: `port` defaults from the scheme.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointInput {
    pub scheme: Scheme,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfigInput {
    pub endpoints: Vec<EndpointInput>,
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub config: BTreeMap<String, Value>,
}

// ---------------------------------------------------------------------------
// Headers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeaderRules,
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

impl HeadersConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.response.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default)]
    pub passthrough: PassthroughMode,
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

impl RequestHeaderRules {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.passthrough == PassthroughMode::None
            && self.passthrough_allowlist.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

impl ResponseHeaderRules {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.add.is_empty() && self.remove.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// One entry of a plugin chain.
///
/// The upstream schema spells items as bare identifier strings while
/// ADR-0009 spells them as `{plugin_ref, config}` objects; both are accepted
/// and the object form is what is serialized back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginBinding {
    pub plugin_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    pub config: BTreeMap<String, Value>,
}

impl PluginBinding {
    #[must_use]
    pub fn new(plugin_ref: String, config: BTreeMap<String, Value>) -> Self {
        let plugin_uuid = gts_helpers::plugin_ref_uuid(&plugin_ref);
        Self {
            plugin_ref,
            plugin_uuid,
            config,
        }
    }
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Ref(String),
            Object {
                #[serde(alias = "plugin_ref", alias = "ref", alias = "id")]
                plugin_ref: String,
                #[serde(default)]
                config: BTreeMap<String, Value>,
            },
        }

        match Raw::deserialize(deserializer)? {
            Raw::Ref(plugin_ref) => Ok(Self::new(plugin_ref, BTreeMap::new())),
            Raw::Object { plugin_ref, config } => Ok(Self::new(plugin_ref, config)),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

impl PluginsConfig {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    pub rate: u32,
    #[serde(default)]
    pub window: RateLimitWindow,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    #[serde(default)]
    pub mode: BudgetMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default)]
    pub burst: BurstConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetConfig>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "one")]
    pub cost: u32,
    #[serde(default = "yes")]
    pub response_headers: bool,
}

fn one() -> u32 {
    1
}

fn yes() -> bool {
    true
}

impl RateLimitConfig {
    /// Bucket capacity — `burst.capacity` when present, else `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate).max(1)
    }

    /// Sustained refill expressed in tokens per second.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        f64::from(self.sustained.rate) / self.sustained.window.seconds() as f64
    }
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    pub enabled: bool,
    #[serde(default)]
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

impl CorsConfig {
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|o| o == "*" || o.eq_ignore_ascii_case(origin))
    }

    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
    }
}

// ---------------------------------------------------------------------------
// Match rules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    pub methods: Vec<String>,
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// `oagw_route.match_type` — which match-key table backs this route.
    #[must_use]
    pub fn match_type(&self) -> &'static str {
        if self.grpc.is_some() { "grpc" } else { "http" }
    }
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub alias: String,
    pub protocol: String,
    pub enabled: bool,
    pub server: ServerConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    pub headers: HeadersConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    pub plugins: PluginsConfig,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Upstream {
    /// Distinct endpoint hostnames, in declaration order.
    #[must_use]
    pub fn hosts(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for ep in &self.server.endpoints {
            if !out.iter().any(|h| h == &ep.host) {
                out.push(ep.host.clone());
            }
        }
        out
    }

    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.protocol == gts_helpers::PROTOCOL_GRPC
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub upstream_id: Uuid,
    pub match_type: String,
    pub priority: i32,
    pub enabled: bool,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    pub plugins: PluginsConfig,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Which trait a custom plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            Self::Auth => gts_helpers::AUTH_PLUGIN_TYPE,
            Self::Guard => gts_helpers::GUARD_PLUGIN_TYPE,
            Self::Transform => gts_helpers::TRANSFORM_PLUGIN_TYPE,
        }
    }

    #[must_use]
    pub fn from_base_type(base: &str) -> Option<Self> {
        match base {
            gts_helpers::AUTH_PLUGIN_TYPE => Some(Self::Auth),
            gts_helpers::GUARD_PLUGIN_TYPE => Some(Self::Guard),
            gts_helpers::TRANSFORM_PLUGIN_TYPE => Some(Self::Transform),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plugin {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub plugin_type: PluginKind,
    pub name: String,
    pub description: Option<String>,
    pub phases: Vec<String>,
    pub config_schema: Map<String, Value>,
    #[serde(skip_serializing)]
    pub source_code: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub gc_eligible_at: Option<String>,
}

impl Plugin {
    /// Anonymous GTS identifier of this plugin.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts_helpers::anonymous_id(self.plugin_type.base_type(), self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_standard_ports_follow_the_design_table() {
        assert_eq!(Scheme::Http.standard_port(), 80);
        assert_eq!(Scheme::Https.standard_port(), 443);
        assert_eq!(Scheme::Wss.standard_port(), 443);
        assert_eq!(Scheme::Wt.standard_port(), 443);
        assert_eq!(Scheme::Grpc.standard_port(), 443);
        assert!(!Scheme::Http.is_tls());
        assert!(Scheme::Wss.is_tls());
    }

    #[test]
    fn http_scheme_deserializes() {
        let ep: EndpointInput =
            serde_json::from_str(r#"{"scheme":"http","host":"127.0.0.1","port":80}"#)
                .expect("http is a legal scheme");
        assert_eq!(ep.scheme, Scheme::Http);
        assert_eq!(ep.port, Some(80));
    }

    #[test]
    fn plugin_binding_accepts_both_spellings() {
        let bare: PluginBinding =
            serde_json::from_str(r#""gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1""#)
                .expect("string form");
        assert!(bare.plugin_uuid.is_none());
        assert!(bare.config.is_empty());

        let full: PluginBinding = serde_json::from_str(
            r#"{"plugin_ref":"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                "config":{"required_request_headers":"x-correlation-id"}}"#,
        )
        .expect("object form");
        assert_eq!(full.config.len(), 1);
    }

    #[test]
    fn plugin_binding_extracts_the_uuid_for_custom_plugins() {
        let id = Uuid::new_v4();
        let raw = format!("\"gts.cf.core.oagw.guard_plugin.v1~{id}\"");
        let b: PluginBinding = serde_json::from_str(&raw).expect("uuid-backed ref");
        assert_eq!(b.plugin_uuid, Some(id));
    }

    #[test]
    fn rate_limit_defaults_match_adr_0003() {
        let rl: RateLimitConfig =
            serde_json::from_str(r#"{"sustained":{"rate":100}}"#).expect("minimal rate limit");
        assert_eq!(rl.algorithm, RateLimitAlgorithm::TokenBucket);
        assert_eq!(rl.sustained.window, RateLimitWindow::Second);
        assert_eq!(rl.scope, RateLimitScope::Tenant);
        assert_eq!(rl.strategy, RateLimitStrategy::Reject);
        assert_eq!(rl.cost, 1);
        assert!(rl.response_headers);
        assert_eq!(rl.capacity(), 100);
        assert!((rl.refill_per_second() - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn window_seconds_are_exact() {
        assert_eq!(RateLimitWindow::Minute.seconds(), 60);
        assert_eq!(RateLimitWindow::Hour.seconds(), 3600);
        assert_eq!(RateLimitWindow::Day.seconds(), 86_400);
    }

    #[test]
    fn cors_origin_matching_is_exact_and_wildcard_aware() {
        let cors = CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: default_cors_methods(),
            expose_headers: vec![],
            allow_credentials: false,
        };
        assert!(cors.allows_origin("https://app.example.com"));
        assert!(!cors.allows_origin("https://app.example.com:8080"));
        assert!(!cors.allows_origin("http://app.example.com"));
        assert!(cors.allows_method("get"));
        assert!(!cors.allows_method("DELETE"));
    }
}
