//! Domain model: upstreams, routes, plugins and the configuration blocks
//! they carry.
//!
//! Field names and value domains follow `docs/schemas/upstream.v1.schema.json`
//! and `docs/schemas/route.v1.schema.json`; the extensions documented in
//! ADR-0003 (`budget`, `response_headers`) and the PRD domain model
//! (`enabled`, `priority`, `tags`) are included alongside them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

/// Sharing mode for a configuration block across the tenant hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; a descendant may override.
    Inherit,
    /// Visible; a descendant may not override.
    Enforce,
}

/// A single upstream server endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// Connection scheme (`https`, `http`, `wss`, `ws`, `wt`, `grpc`).
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_scheme() -> String {
    "https".to_owned()
}

const fn default_port() -> u16 {
    443
}

impl Endpoint {
    /// `true` when the scheme denotes a plaintext transport.
    #[must_use]
    pub fn is_plaintext(&self) -> bool {
        matches!(self.scheme.as_str(), "http" | "ws")
    }

    /// The default port for this endpoint's scheme (`80` for plaintext HTTP,
    /// `443` otherwise), used when deriving an alias.
    #[must_use]
    pub fn standard_port(&self) -> u16 {
        if self.scheme == "http" || self.scheme == "ws" {
            80
        } else {
            443
        }
    }
}

/// Server block: the load-balance pool for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServerConfig {
    /// One or more endpoints forming a round-robin pool.
    pub endpoints: Vec<Endpoint>,
}

/// Auth plugin binding on an upstream.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: Map<String, Value>,
}

/// Which inbound headers reach the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward none (default — an inbound header must be opted in).
    #[default]
    None,
    /// Forward only the names in `passthrough_allowlist`.
    Allowlist,
    /// Forward everything that is not routing or hop-by-hop.
    All,
}

/// Request-direction header transformation rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RequestHeaderRules {
    /// Headers to set (overwriting any existing value).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append (duplicates allowed).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to drop from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Passthrough policy.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Names forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-direction header transformation rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResponseHeaderRules {
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

/// Header transformation configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeadersConfig {
    /// Request-direction rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    /// Response-direction rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// One entry of a plugin chain.
///
/// Accepts either the compact form (`"gts.…required_headers.v1"`) or the
/// object form (`{"plugin_ref": "…", "config": {…}}`, ADR-0009); both
/// normalize to the object form on the way out.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(from = "PluginBindingWire")]
pub struct PluginBinding {
    /// Canonical plugin identifier (named GTS id, or `<type>~<uuid>`).
    pub plugin_ref: String,
    /// Plugin-specific configuration.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    #[schema(value_type = Object)]
    pub config: Map<String, Value>,
}

/// Wire shape accepted for a [`PluginBinding`].
#[derive(Deserialize)]
#[serde(untagged)]
enum PluginBindingWire {
    /// `"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"`
    Compact(String),
    /// `{"plugin_ref": "…", "config": {…}}`
    Expanded {
        #[serde(alias = "plugin_id", alias = "ref", alias = "id")]
        plugin_ref: String,
        #[serde(default)]
        config: Map<String, Value>,
    },
}

impl From<PluginBindingWire> for PluginBinding {
    fn from(value: PluginBindingWire) -> Self {
        match value {
            PluginBindingWire::Compact(plugin_ref) => Self {
                plugin_ref,
                config: Map::new(),
            },
            PluginBindingWire::Expanded { plugin_ref, config } => Self { plugin_ref, config },
        }
    }
}

/// Plugin chain attached to an upstream or route.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered chain; upstream entries run before route entries.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Rate limit window unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    /// Per second (default).
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
    pub fn seconds(self) -> f64 {
        match self {
            RateWindow::Second => 1.0,
            RateWindow::Minute => 60.0,
            RateWindow::Hour => 3600.0,
            RateWindow::Day => 86_400.0,
        }
    }
}

/// Sustained-rate leg of the dual-rate configuration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window unit.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst leg of the dual-rate configuration.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BurstRate {
    /// Bucket capacity; defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    /// Answer `429` with `Retry-After` (default).
    #[default]
    Reject,
    /// Wait for capacity within a bounded budget.
    Queue,
    /// Serve the request, flagged as degraded.
    Degrade,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket — allows bursts up to capacity (default).
    #[default]
    TokenBucket,
    /// Sliding window — no boundary burst.
    SlidingWindow,
}

/// Hierarchical budget allocation mode (ADR-0003 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    /// No budget tracking (default).
    #[default]
    Unlimited,
    /// The parent allocates a fixed budget to children.
    Allocated,
    /// Children share the parent's budget.
    Shared,
}

/// Hierarchical budget allocation.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BudgetConfig {
    /// Allocation mode.
    #[serde(default)]
    pub mode: BudgetMode,
    /// Total budget available to descendants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// Permitted overcommit ratio (1.0 – 2.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
}

/// Rate limit configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate (required).
    pub sustained: SustainedRate,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstRate>,
    /// Hierarchical budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetConfig>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Exceeded-limit behaviour.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Emit `X-RateLimit-*` response headers.
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

const fn default_cost() -> u32 {
    1
}

const fn default_true() -> bool {
    true
}

impl RateLimitConfig {
    /// Effective bucket capacity.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst
            .and_then(|b| b.capacity)
            .unwrap_or(self.sustained.rate)
            .max(1)
    }

    /// Refill rate in tokens per second.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn refill_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.seconds()
    }
}

/// CORS configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Master switch (secure default: disabled).
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins; `["*"]` for any.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods; defaults to `GET, POST`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_methods: Option<Vec<String>>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Allow credentialed requests (incompatible with `*`).
    #[serde(default)]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Allowed methods with the documented default applied.
    #[must_use]
    pub fn effective_methods(&self) -> Vec<String> {
        self.allowed_methods
            .clone()
            .unwrap_or_else(|| vec!["GET".to_owned(), "POST".to_owned()])
    }

    /// `true` when `origin` passes the (exact, case-sensitive) origin check.
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }
}

/// How the inbound `/{path_suffix}` is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject requests carrying a suffix beyond `match.http.path`.
    Disabled,
    /// Append the remaining suffix to `match.http.path` (default).
    #[default]
    Append,
}

/// HTTP match rules.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpMatch {
    /// Methods this route serves.
    pub methods: Vec<String>,
    /// Path prefix matched against the inbound path suffix.
    pub path: String,
    /// Permitted query parameter names; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Path-suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (catalogued; Phase 3).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrpcMatch {
    /// Fully-qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped inbound match rules; exactly one variant is populated.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct MatchConfig {
    /// HTTP rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// The mutable part of an upstream — what a `POST` or `PUT` body carries.
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    /// Enable flag (default `true`).
    pub enabled: bool,
    /// Routing alias (normalized, derived or explicit).
    pub alias: String,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol GTS identifier.
    pub protocol: String,
    /// Auth plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Guard/transform plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    pub cors: Option<CorsConfig>,
}

/// A persisted upstream.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Creation timestamp (RFC 3339, UTC).
    pub created_at: String,
    /// Last-modification timestamp (RFC 3339, UTC).
    pub updated_at: String,
    /// Mutable configuration.
    pub spec: UpstreamSpec,
}

impl Upstream {
    /// Alias of this upstream.
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.spec.alias
    }

    /// The auth plugin's canonical reference, if one is bound.
    #[must_use]
    pub fn auth_plugin_ref(&self) -> Option<&str> {
        self.spec
            .auth
            .as_ref()
            .and_then(|a| a.plugin_type.as_deref())
    }
}

/// The mutable part of a route.
#[derive(Debug, Clone)]
pub struct RouteSpec {
    /// Enable flag (default `true`).
    pub enabled: bool,
    /// Match priority; higher wins on an equal-length path match.
    pub priority: i32,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Inbound match rules.
    pub match_config: MatchConfig,
    /// Guard/transform plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy override.
    pub cors: Option<CorsConfig>,
}

/// A persisted route.
#[derive(Debug, Clone)]
pub struct Route {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent upstream (immutable after creation).
    pub upstream_id: Uuid,
    /// Creation timestamp (RFC 3339, UTC).
    pub created_at: String,
    /// Last-modification timestamp (RFC 3339, UTC).
    pub updated_at: String,
    /// Mutable configuration.
    pub spec: RouteSpec,
}

impl Route {
    /// The HTTP match block, when this is an HTTP route.
    #[must_use]
    pub fn http(&self) -> Option<&HttpMatch> {
        self.spec.match_config.http.as_ref()
    }

    /// Key used for the "no two enabled routes share this" invariant.
    #[must_use]
    pub fn match_key(&self) -> String {
        match (&self.spec.match_config.http, &self.spec.match_config.grpc) {
            (Some(http), _) => {
                let mut methods: Vec<String> = http
                    .methods
                    .iter()
                    .map(|m| m.to_ascii_uppercase())
                    .collect();
                methods.sort();
                format!(
                    "http|{}|{}|{}",
                    http.path,
                    self.spec.priority,
                    methods.join(",")
                )
            }
            (None, Some(grpc)) => {
                format!(
                    "grpc|{}|{}|{}",
                    grpc.service, grpc.method, self.spec.priority
                )
            }
            (None, None) => format!("none|{}", self.spec.priority),
        }
    }
}

/// Plugin kind, as encoded in the plugin's base GTS type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Credential injection; one per upstream.
    Auth,
    /// Validation and policy enforcement; may reject.
    Guard,
    /// Request/response mutation.
    Transform,
}

impl PluginKind {
    /// Base GTS type for this kind.
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            PluginKind::Auth => crate::domain::gts_helpers::AUTH_PLUGIN_TYPE,
            PluginKind::Guard => crate::domain::gts_helpers::GUARD_PLUGIN_TYPE,
            PluginKind::Transform => crate::domain::gts_helpers::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Recover the kind from a plugin reference's base type.
    #[must_use]
    pub fn from_plugin_ref(plugin_ref: &str) -> Option<Self> {
        let (base, _) = crate::domain::gts_helpers::split_plugin_ref(plugin_ref)?;
        [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform]
            .into_iter()
            .find(|kind| base.eq_ignore_ascii_case(kind.base_type()))
    }
}

/// Transform plugin phase.
///
/// The `On*` prefix mirrors the hook names a plugin author writes
/// (`on_request`, `on_response`, `on_error`), so it stays.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginPhase {
    /// Before the upstream call.
    OnRequest,
    /// After a successful upstream call.
    OnResponse,
    /// After a failed upstream call.
    OnError,
}

/// A tenant-defined custom plugin. Immutable after creation.
#[derive(Debug, Clone)]
pub struct Plugin {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin kind.
    pub kind: PluginKind,
    /// Tenant-unique name.
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// Declared transform phases.
    pub phases: Vec<PluginPhase>,
    /// JSON Schema for the plugin's configuration.
    pub config_schema: Option<Value>,
    /// Starlark source.
    pub source_code: String,
    /// Creation timestamp (RFC 3339, UTC).
    pub created_at: String,
    /// Last time the plugin was resolved on the hot path.
    pub last_used_at: Option<String>,
    /// Instant from which an unlinked plugin may be garbage-collected.
    pub gc_eligible_at: Option<String>,
}

impl Plugin {
    /// The plugin's canonical GTS identifier.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts_helpers::anonymous_id(self.kind.base_type(), self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CorsConfig, Endpoint, PassthroughMode, PluginBinding, PluginKind, PluginsConfig,
        RateLimitConfig, RateWindow, SharingMode,
    };

    #[test]
    fn plugin_binding_accepts_compact_and_expanded() {
        let compact: PluginBinding = serde_json::from_str(
            "\"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1\"",
        )
        .expect("compact form");
        assert_eq!(
            compact.plugin_ref,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert!(compact.config.is_empty());

        let expanded: PluginBinding = serde_json::from_value(serde_json::json!({
            "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "config": { "required_request_headers": "x-correlation-id" }
        }))
        .expect("expanded form");
        assert_eq!(
            expanded.config["required_request_headers"],
            "x-correlation-id"
        );
    }

    #[test]
    fn plugins_config_normalizes_to_objects() {
        let cfg: PluginsConfig = serde_json::from_value(serde_json::json!({
            "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"]
        }))
        .expect("parses");
        assert_eq!(cfg.sharing, SharingMode::Private);
        let json = serde_json::to_value(&cfg).expect("serializes");
        assert!(json["items"][0]["plugin_ref"].is_string());
    }

    #[test]
    fn rate_limit_defaults_follow_adr_0003() {
        let cfg: RateLimitConfig =
            serde_json::from_value(serde_json::json!({ "sustained": { "rate": 100 } }))
                .expect("parses");
        assert_eq!(cfg.sustained.window, RateWindow::Second);
        assert_eq!(cfg.cost, 1);
        assert!(cfg.response_headers);
        assert_eq!(cfg.capacity(), 100, "burst defaults to sustained.rate");
        assert!((cfg.refill_per_second() - 100.0).abs() < f64::EPSILON);

        let per_minute: RateLimitConfig = serde_json::from_value(
            serde_json::json!({ "sustained": { "rate": 60, "window": "minute" } }),
        )
        .expect("parses");
        assert!((per_minute.refill_per_second() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn passthrough_defaults_to_none() {
        assert_eq!(PassthroughMode::default(), PassthroughMode::None);
    }

    #[test]
    fn endpoint_scheme_classification() {
        let plaintext = Endpoint {
            scheme: "http".to_owned(),
            host: "localhost".to_owned(),
            port: 80,
        };
        assert!(plaintext.is_plaintext());
        assert_eq!(plaintext.standard_port(), 80);

        let tls = Endpoint {
            scheme: "https".to_owned(),
            host: "api.openai.com".to_owned(),
            port: 443,
        };
        assert!(!tls.is_plaintext());
        assert_eq!(tls.standard_port(), 443);
    }

    #[test]
    fn cors_origin_matching_is_exact() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            ..CorsConfig::default()
        };
        assert!(cors.origin_allowed("https://app.example.com"));
        assert!(!cors.origin_allowed("http://app.example.com"));
        assert!(!cors.origin_allowed("https://app.example.com:8080"));
        assert_eq!(cors.effective_methods(), vec!["GET", "POST"]);
    }

    #[test]
    fn plugin_kind_from_ref() {
        assert_eq!(
            PluginKind::from_plugin_ref("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
            Some(PluginKind::Auth)
        );
        assert_eq!(PluginKind::from_plugin_ref("nonsense"), None);
    }
}
