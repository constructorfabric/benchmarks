//! Domain model — the configuration entities OAGW owns.
//!
//! The shapes here mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` field for field, so the same types are
//! usable as wire representations without a second, drifting copy.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::gts;

/// Free-form configuration blob handed to a plugin.
pub type ConfigMap = BTreeMap<String, serde_json::Value>;

// ---------------------------------------------------------------------------
// Shared enums
// ---------------------------------------------------------------------------

/// Visibility of a configuration field across the tenant hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
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
    #[must_use]
    pub fn is_visible_to_descendants(self) -> bool {
        matches!(self, Self::Inherit | Self::Enforce)
    }

    #[must_use]
    pub fn is_enforced(self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Transport scheme of an upstream endpoint.
///
/// The TLS family is what `cpt-cf-oagw-constraint-https-only` describes as the
/// default posture. The plaintext members are accepted by the management API
/// regardless; whether a plaintext connection is actually dialled is decided at
/// proxy time by `allow_http_upstream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Http,
    Https,
    Ws,
    Wss,
    /// WebTransport.
    Wt,
    Grpc,
}

impl Default for Scheme {
    fn default() -> Self {
        Self::Https
    }
}

impl Scheme {
    /// Whether a connection with this scheme is wrapped in TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http | Self::Ws)
    }

    /// Port omitted from a derived alias (`docs/DESIGN.md` §"Standard ports").
    #[must_use]
    pub fn standard_port(self) -> u16 {
        if self.is_tls() { 443 } else { 80 }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
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

/// Wire protocol spoken to the upstream. Serialized as its GTS identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum Protocol {
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    #[must_use]
    pub fn as_gts_id(self) -> &'static str {
        match self {
            Self::Http => gts::PROTOCOL_HTTP,
            Self::Grpc => gts::PROTOCOL_GRPC,
        }
    }
}

/// Which inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward nothing beyond what OAGW itself sets.
    #[default]
    None,
    /// Forward only the headers named in `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header that is not routing- or hop-by-hop.
    All,
}

/// How the `/{path_suffix}` part of a proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// A suffix beyond the matched route path is rejected.
    Disabled,
    /// The suffix is appended to the route path.
    #[default]
    Append,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Counter scope for a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// `429 Too Many Requests` with `Retry-After`.
    #[default]
    Reject,
    /// Wait for capacity within a bounded budget, then reject.
    Queue,
    /// Serve with reduced functionality (currently: forward without waiting).
    Degrade,
}

/// Replenishment window for a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    #[must_use]
    pub fn seconds(self) -> f64 {
        match self {
            Self::Second => 1.0,
            Self::Minute => 60.0,
            Self::Hour => 3600.0,
            Self::Day => 86_400.0,
        }
    }
}

/// The three plugin kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            Self::Auth => gts::AUTH_PLUGIN_BASE,
            Self::Guard => gts::GUARD_PLUGIN_BASE,
            Self::Transform => gts::TRANSFORM_PLUGIN_BASE,
        }
    }

    #[must_use]
    pub fn from_base_type(base: &str) -> Option<Self> {
        match base {
            gts::AUTH_PLUGIN_BASE => Some(Self::Auth),
            gts::GUARD_PLUGIN_BASE => Some(Self::Guard),
            gts::TRANSFORM_PLUGIN_BASE => Some(Self::Transform),
            _ => None,
        }
    }
}

/// Phase a transform plugin participates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginPhase {
    OnRequest,
    OnResponse,
    OnError,
}

// ---------------------------------------------------------------------------
// Value objects
// ---------------------------------------------------------------------------

/// One member of an upstream's load-balance pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    #[serde(default)]
    pub scheme: Scheme,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    443
}

impl Endpoint {
    /// `host` for a standard port, `host:port` otherwise.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.port == self.scheme.standard_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// The upstream's endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

/// Auth plugin binding for an upstream. At most one per upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: ConfigMap,
}

/// Request-phase header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
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

/// Response-phase header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation rules for an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeaderRules,
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

/// A single plugin binding: the plugin identifier plus its per-binding config.
///
/// Accepts both the object form documented in ADR 0009
/// (`{"plugin_ref": "...", "config": {...}}`) and the bare-string form of
/// `schemas/upstream.v1.schema.json`; it always serializes as the object form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, utoipa::ToSchema)]
pub struct PluginBinding {
    /// Canonical plugin identifier — a full GTS id, or a bare UUID for a
    /// custom plugin.
    pub plugin_ref: String,
    /// Extracted UUID when `plugin_ref` is UUID-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: ConfigMap,
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Ref(String),
            Object {
                #[serde(alias = "ref", alias = "id", alias = "type")]
                plugin_ref: String,
                #[serde(default)]
                config: ConfigMap,
            },
        }

        let binding = match Repr::deserialize(deserializer)? {
            Repr::Ref(plugin_ref) => Self {
                plugin_ref,
                plugin_uuid: None,
                config: ConfigMap::new(),
            },
            Repr::Object { plugin_ref, config } => Self {
                plugin_ref,
                plugin_uuid: None,
                config,
            },
        };
        Ok(binding.with_derived_uuid())
    }
}

impl PluginBinding {
    /// Fill [`Self::plugin_uuid`] from the instance part of `plugin_ref`.
    #[must_use]
    pub fn with_derived_uuid(mut self) -> Self {
        self.plugin_uuid = gts::instance_uuid(&self.plugin_ref);
        self
    }
}

/// An ordered plugin chain attached to an upstream or route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Sustained replenishment rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    pub rate: u32,
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst allowance (token bucket capacity).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BurstCapacity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Hierarchical budget allocation (ADR 0003).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<BudgetMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
}

/// Budget allocation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetMode {
    #[default]
    Unlimited,
    Allocated,
    Shared,
}

/// Dual-rate token bucket configuration (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedRate,
    #[serde(default)]
    pub burst: BurstCapacity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<RateLimitBudget>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_cost")]
    pub cost: u32,
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

fn default_cost() -> u32 {
    1
}

pub(crate) fn default_true() -> bool {
    true
}

impl RateLimitConfig {
    /// Sustained tokens per second.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        f64::from(self.sustained.rate) / self.sustained.window.seconds()
    }

    /// Bucket capacity, defaulting to the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate).max(1)
    }

    /// Take the stricter of two limits, field by field
    /// (`effective = min(ancestor.enforced, descendant)`).
    #[must_use]
    pub fn stricter_of(a: Self, b: Self) -> Self {
        let mut out = if a.refill_per_second() <= b.refill_per_second() {
            a
        } else {
            b
        };
        out.burst = BurstCapacity {
            capacity: Some(a.capacity().min(b.capacity())),
        };
        out.cost = a.cost.max(b.cost);
        out
    }
}

/// CORS policy (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
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
    /// Exact, case-sensitive origin match (no regex — ADR 0004).
    #[must_use]
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }

    #[must_use]
    pub fn method_allowed(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }

    #[must_use]
    pub fn has_wildcard_origin(&self) -> bool {
        self.allowed_origins.iter().any(|o| o == "*")
    }
}

/// HTTP inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    pub methods: Vec<String>,
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl HttpMatch {
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }

    #[must_use]
    pub fn allows_query_param(&self, name: &str) -> bool {
        self.query_allowlist.iter().any(|p| p == name)
    }
}

/// gRPC inbound matching rules (Phase 3 — stored, not yet routable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Protocol-scoped match rules. Exactly one member must be present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    #[must_use]
    pub fn match_type(&self) -> &'static str {
        if self.grpc.is_some() { "grpc" } else { "http" }
    }
}

// ---------------------------------------------------------------------------
// Aggregates
// ---------------------------------------------------------------------------

/// Tenant-scoped root configuration object for one external service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub alias: String,
    pub enabled: bool,
    pub protocol: Protocol,
    pub server: ServerConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    pub tags: Vec<String>,
}

impl Upstream {
    /// Anonymous GTS identifier for this upstream.
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", gts::UPSTREAM_BASE, self.id)
    }

    /// Endpoint whose host matches `host`, case-insensitively.
    #[must_use]
    pub fn endpoint_for_host(&self, host: &str) -> Option<&Endpoint> {
        self.server
            .endpoints
            .iter()
            .find(|e| e.host.eq_ignore_ascii_case(host))
    }

    /// Distinct endpoint hostnames, in declaration order.
    #[must_use]
    pub fn endpoint_hosts(&self) -> Vec<String> {
        self.server
            .endpoints
            .iter()
            .map(|e| e.host.clone())
            .collect()
    }
}

/// An API path on an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub upstream_id: Uuid,
    pub enabled: bool,
    pub priority: i32,
    pub r#match: MatchConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    pub tags: Vec<String>,
}

impl Route {
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", gts::ROUTE_BASE, self.id)
    }

    #[must_use]
    pub fn http(&self) -> Option<&HttpMatch> {
        self.r#match.http.as_ref()
    }
}

/// A tenant-defined custom (Starlark) plugin. Immutable after creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginDef {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub plugin_type: PluginKind,
    pub name: String,
    pub description: Option<String>,
    pub phases: Vec<PluginPhase>,
    pub config_schema: Option<serde_json::Value>,
    pub source_code: String,
    /// Epoch seconds of the last proxy request that resolved this plugin.
    pub last_used_at: Option<u64>,
    /// Epoch seconds after which an unlinked plugin may be collected.
    pub gc_eligible_at: Option<u64>,
}

impl PluginDef {
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", self.plugin_type.base_type(), self.id)
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
