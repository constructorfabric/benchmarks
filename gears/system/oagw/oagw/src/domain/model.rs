//! Domain entities.
//!
//! The shapes here mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` — the serde representation *is* the
//! wire representation, so a round-trip through the management API is
//! lossless.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

use super::gts_helpers;

/// Visibility of a configuration field across the tenant hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; a descendant may override.
    Inherit,
    /// Visible; a descendant may not override.
    Enforce,
}

impl SharingMode {
    /// Whether descendants can see the field at all.
    #[must_use]
    pub const fn is_visible(self) -> bool {
        matches!(self, Self::Inherit | Self::Enforce)
    }

    /// Whether descendants are forbidden from overriding the field.
    #[must_use]
    pub const fn is_enforced(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Transport scheme of an upstream endpoint.
///
/// `docs/schemas/upstream.v1.schema.json` enumerates the TLS family only,
/// because HTTPS-only is the *default* posture
/// (`cpt-cf-oagw-constraint-https-only`). The plaintext members are accepted
/// here as legal field values; whether a plaintext connection is actually
/// opened is decided at connect time by `allow_http_upstream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS. The schema default.
    #[default]
    Https,
    /// Plaintext WebSocket.
    Ws,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over HTTP/2.
    Grpc,
}

impl Scheme {
    /// Port omitted from a derived alias for this scheme.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Whether a connection using this scheme is TLS-wrapped.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Wt | Self::Grpc)
    }

    /// Wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Ws => "ws",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Transport scheme.
    pub scheme: Scheme,
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port; defaults to the scheme's standard port.
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port: the declared one, else the scheme default.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.standard_port())
    }

    /// `host:port`, omitting the port when it is the scheme default.
    #[must_use]
    pub fn authority(&self) -> String {
        let port = self.effective_port();
        if port == self.scheme.standard_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, port)
        }
    }
}

/// The pool of endpoints backing an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// One or more endpoints; multiple entries form a load-balance pool.
    pub endpoints: Vec<Endpoint>,
}

/// Outbound authentication for an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_ref: Option<String>,
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// Which inbound request headers reach the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward nothing (the secure default).
    #[default]
    None,
    /// Forward only names in `passthrough_allowlist`.
    Allowlist,
    /// Forward everything except routing and hop-by-hop headers.
    All,
}

/// Request-direction header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeadersConfig {
    /// Headers to set, replacing any existing value.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append, keeping existing values.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to drop from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Passthrough policy for inbound headers.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Names forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-direction header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseHeadersConfig {
    /// Headers to set on the response to the client.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append to the response.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation rules for both directions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadersConfig {
    /// Request-direction rules.
    #[serde(default)]
    pub request: RequestHeadersConfig,
    /// Response-direction rules.
    #[serde(default)]
    pub response: ResponseHeadersConfig,
}

/// One entry of a plugin chain.
///
/// The upstream schema types `plugins.items[]` as a bare identifier string
/// while `ADR/0009-required-headers-guard-plugin.md` shows the object form
/// with a per-binding `config`. Both are accepted on input; the object form
/// is what is written back out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PluginBinding {
    /// Canonical plugin identifier (named GTS id, or a UUID-backed id).
    pub plugin_ref: String,
    /// Binding-scoped plugin configuration.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    pub config: serde_json::Map<String, serde_json::Value>,
}

impl PluginBinding {
    /// Binding with no configuration.
    #[must_use]
    pub fn named(plugin_ref: impl Into<String>) -> Self {
        Self {
            plugin_ref: plugin_ref.into(),
            config: serde_json::Map::new(),
        }
    }

    /// UUID of the custom plugin this binding points at, when UUID-backed.
    #[must_use]
    pub fn plugin_uuid(&self) -> Option<Uuid> {
        gts_helpers::plugin_ref_uuid(&self.plugin_ref)
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
            Ref(String),
            Object {
                plugin_ref: String,
                #[serde(default)]
                config: serde_json::Map<String, serde_json::Value>,
            },
        }

        match Raw::deserialize(deserializer)? {
            Raw::Ref(plugin_ref) => Ok(Self::named(plugin_ref)),
            Raw::Object { plugin_ref, config } => Ok(Self { plugin_ref, config }),
        }
    }
}

/// An ordered plugin chain plus its sharing mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Hierarchical sharing mode for the chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Bindings in execution order.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Replenishment window for a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Window length in seconds.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained replenishment rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Length of the replenishment window.
    #[serde(default)]
    pub window: RateWindow,
}

impl SustainedRate {
    /// Replenishment expressed in tokens per second.
    #[must_use]
    pub fn per_second(self) -> f64 {
        f64::from(self.rate) / self.window.as_secs() as f64
    }
}

/// Burst allowance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BurstConfig {
    /// Bucket capacity; defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket — bursts allowed up to capacity.
    #[default]
    TokenBucket,
    /// Sliding window — no boundary burst.
    SlidingWindow,
}

/// Counter scope for a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    /// One counter for the whole deployment.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    /// Reject with `429` and a `Retry-After` header.
    #[default]
    Reject,
    /// Wait for capacity within a bounded budget, then reject.
    Queue,
    /// Forward with reduced functionality (marked for the upstream).
    Degrade,
}

/// Budget allocation mode (`ADR/0003-rate-limiting.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    /// No budget tracking.
    #[default]
    Unlimited,
    /// Parent allocates a fixed budget to children.
    Allocated,
    /// Children share the parent's budget first-come-first-served.
    Shared,
}

/// Hierarchical budget allocation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Allocation mode.
    #[serde(default)]
    pub mode: BudgetMode,
    /// Total budget for the subtree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// Permitted overcommit ratio over `total`.
    #[serde(default = "default_overcommit")]
    pub overcommit_ratio: f64,
}

fn default_overcommit() -> f64 {
    1.0
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            mode: BudgetMode::default(),
            total: None,
            overcommit_ratio: default_overcommit(),
        }
    }
}

/// Dual-rate limit configuration (`ADR/0003-rate-limiting.md` §2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained replenishment.
    pub sustained: SustainedRate,
    /// Burst allowance.
    #[serde(default)]
    pub burst: BurstConfig,
    /// Budget allocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetConfig>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour on exhaustion.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Emit `X-RateLimit-*` headers.
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

fn default_cost() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

impl RateLimitConfig {
    /// Effective bucket capacity.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate).max(1)
    }
}

/// CORS policy for an upstream or route (`ADR/0004-cors.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Master switch.
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are permitted.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// An upstream service definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key used in `/oagw/v1/proxy/{alias}/…`.
    pub alias: String,
    /// Whether proxy traffic is accepted.
    pub enabled: bool,
    /// Discovery tags (add-only across the hierarchy).
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol GTS identifier.
    pub protocol: String,
    /// Outbound authentication.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: HeadersConfig,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    /// `true` when this upstream speaks HTTP (the only proxied protocol).
    #[must_use]
    pub fn is_http(&self) -> bool {
        self.protocol == gts_helpers::PROTOCOL_HTTP
    }

    /// The endpoint pool's shared scheme.
    #[must_use]
    pub fn scheme(&self) -> Option<Scheme> {
        self.server.endpoints.first().map(|e| e.scheme)
    }
}

/// How a proxy-URL path suffix is combined with the route path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject requests that carry a suffix.
    Disabled,
    /// Append the suffix to the route path (default).
    #[default]
    Append,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMatch {
    /// Accepted HTTP methods.
    pub methods: Vec<String>,
    /// Route path prefix on the upstream.
    pub path: String,
    /// Permitted query parameter names; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Path-suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (catalogued; no proxy code path — Phase 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    /// Fully-qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules; exactly one member is present.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MatchConfig {
    /// HTTP match keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A route on an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Upstream this route belongs to.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Tie-breaker; higher wins for equally specific paths.
    pub priority: i32,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Plugin chain appended after the upstream's.
    pub plugins: PluginsConfig,
    /// Route-level rate limit override.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    pub cors: Option<CorsConfig>,
}

/// Which trait a custom plugin implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Credential injection.
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request / response mutation.
    Transform,
}

impl PluginKind {
    /// Base GTS type for this kind.
    #[must_use]
    pub const fn base_type(self) -> &'static str {
        match self {
            Self::Auth => gts_helpers::AUTH_PLUGIN_TYPE,
            Self::Guard => gts_helpers::GUARD_PLUGIN_TYPE,
            Self::Transform => gts_helpers::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Recover the kind from a plugin base type.
    #[must_use]
    pub fn from_base_type(base: &str) -> Option<Self> {
        match base {
            gts_helpers::AUTH_PLUGIN_TYPE => Some(Self::Auth),
            gts_helpers::GUARD_PLUGIN_TYPE => Some(Self::Guard),
            gts_helpers::TRANSFORM_PLUGIN_TYPE => Some(Self::Transform),
            _ => None,
        }
    }

    /// Wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}

/// Lifecycle phase a transform plugin participates in.
///
/// The `On` prefix is the wire vocabulary from
/// `ADR/0002-plugin-system.md` (`on_request`, `on_response`, `on_error`).
#[allow(clippy::enum_variant_names, reason = "the prefix is part of the wire contract")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginPhase {
    /// Before the upstream call.
    OnRequest,
    /// After a successful upstream call.
    OnResponse,
    /// After a failed upstream call.
    OnError,
}

/// A tenant-defined plugin definition.
///
/// Definitions are immutable after creation — updates create a new plugin and
/// re-bind references (`cpt-cf-oagw-principle-plugin-immutable`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Unique-per-tenant name.
    pub name: String,
    /// Free-form description.
    pub description: Option<String>,
    /// Which trait the plugin implements.
    pub plugin_type: PluginKind,
    /// Declared phases (transform plugins).
    pub phases: Vec<PluginPhase>,
    /// JSON Schema constraining binding configuration.
    pub config_schema: serde_json::Value,
    /// Plugin body. Stored verbatim and served by `GET …/{id}/source`.
    pub source_code: String,
    /// Monotonic seconds-since-epoch of the last proxy use, when used.
    pub last_used_at: Option<u64>,
    /// Seconds-since-epoch after which an unlinked plugin is collectable.
    pub gc_eligible_at: Option<u64>,
}

impl Plugin {
    /// This plugin's anonymous GTS identifier.
    #[must_use]
    pub fn gts_id(&self) -> String {
        gts_helpers::anonymous_id(self.plugin_type.base_type(), self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_authority_omits_standard_port() {
        let e = Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".to_owned(),
            port: Some(443),
        };
        assert_eq!(e.authority(), "api.openai.com");

        let e = Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".to_owned(),
            port: Some(8443),
        };
        assert_eq!(e.authority(), "api.openai.com:8443");
    }

    #[test]
    fn plaintext_scheme_deserializes_and_defaults_to_port_80() {
        let e: Endpoint =
            serde_json::from_value(serde_json::json!({"scheme": "http", "host": "localhost"}))
                .expect("http is a legal scheme");
        assert_eq!(e.scheme, Scheme::Http);
        assert_eq!(e.effective_port(), 80);
        assert!(!e.scheme.is_tls());
    }

    #[test]
    fn plugin_binding_accepts_both_spellings() {
        let bare: PluginBinding =
            serde_json::from_value(serde_json::json!(gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID))
                .expect("bare string binding");
        assert_eq!(
            bare.plugin_ref,
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID
        );
        assert!(bare.config.is_empty());

        let object: PluginBinding = serde_json::from_value(serde_json::json!({
            "plugin_ref": gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "config": { "required_request_headers": "x-correlation-id" }
        }))
        .expect("object binding");
        assert_eq!(object.config.len(), 1);
    }

    #[test]
    fn sustained_rate_per_second_conversion() {
        let r = SustainedRate {
            rate: 120,
            window: RateWindow::Minute,
        };
        assert!((r.per_second() - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_limit_capacity_defaults_to_sustained_rate() {
        let cfg = RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 10,
                window: RateWindow::Second,
            },
            burst: BurstConfig { capacity: None },
            budget: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
            response_headers: true,
        };
        assert_eq!(cfg.capacity(), 10);
    }

    #[test]
    fn plugin_kind_round_trips_through_base_type() {
        for kind in [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform] {
            assert_eq!(PluginKind::from_base_type(kind.base_type()), Some(kind));
        }
    }
}
