//! Domain records for the OAGW control plane.
//!
//! Records mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` and are used both as API DTOs
//! (the wire format) and as the in-memory persistence shape. The
//! server-managed fields (`id`, `tenant_id`, `created_at`,
//! `updated_at`) are filled by the service layer on create.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_empty_vec<T>() -> Vec<T> {
    Vec::new()
}

fn default_second_window() -> RateWindow {
    RateWindow::Second
}

fn default_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

fn default_suffix_mode() -> PathSuffixMode {
    PathSuffixMode::Append
}

fn default_token_bucket() -> Algorithm {
    Algorithm::TokenBucket
}

fn default_private() -> SharingMode {
    SharingMode::Private
}

fn default_tenant_scope() -> RateScope {
    RateScope::Tenant
}

fn default_reject() -> RateStrategy {
    RateStrategy::Reject
}

fn default_cost() -> u64 {
    1
}

fn now_millis() -> i64 {
    // `Date.now()`-style source for record timestamps. Kept as an
    // explicit helper so tests can override the clock if ever needed.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// =====================================================================
//                             Upstream
// =====================================================================

/// One upstream endpoint (scheme/host/port). `port` is optional on the
/// wire: it defaults per scheme (HTTP→80, HTTPS/WSS/WT/gRPC→443).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct Endpoint {
    pub scheme: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Resolve the effective port for this endpoint.
    pub fn resolved_port(&self) -> u16 {
        self.port.unwrap_or_else(|| default_port(self.scheme.as_str()))
    }

    /// Whether `host` is an IP literal.
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }

    /// The standard port for the given scheme (HTTP→80, everything
    /// else→443). Unknown schemes treat as 443.
    pub fn is_standard_port(&self) -> bool {
        self.resolved_port() == default_port(self.scheme.as_str())
    }
}

/// Default (standard) port for a scheme.
pub fn default_port(scheme: &str) -> u16 {
    match scheme {
        "http" | "ws" => 80,
        "https" | "wss" | "wt" | "grpc" => 443,
        _ => 443,
    }
}

/// Upstream server pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamServer {
    pub endpoints: Vec<Endpoint>,
}

/// Auth configuration for an upstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamAuth {
    /// Auth plugin GTS identifier, e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
    #[serde(rename = "type")]
    pub plugin_type: String,
    #[serde(default = "default_private")]
    pub sharing: SharingMode,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

/// Plugin references bound to an upstream or route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginList {
    #[serde(default = "default_private")]
    pub sharing: SharingMode,
    #[serde(default = "default_empty_vec")]
    pub items: Vec<serde_json::Value>,
}

/// One resolved plugin reference from a `plugins.items` entry.
///
/// The wire accepts either a plain GTS identifier string or an object
/// `{ "plugin_ref": "...", "config": {...} }` (ADR 0009 example).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRef {
    /// Canonical GTS identifier (`gts.cf.core.oagw.{kind}_plugin.v1~...`).
    pub plugin_ref: String,
    /// Per-binding configuration (merged with any plugin-level config).
    pub config: serde_json::Value,
}

impl PluginRef {
    /// The canonical GTS type prefix before the `~name.v1` tail
    /// (e.g. `gts.cf.core.oagw.guard_plugin.v1`).
    fn type_prefix(&self) -> &str {
        self.plugin_ref
            .rsplit_once('~')
            .map(|(prefix, _)| prefix)
            .unwrap_or("")
    }

    /// Whether this ref declares the guard plugin kind
    /// (`...guard_plugin.v1~`).
    pub fn is_guard_kind(&self) -> bool {
        self.type_prefix().ends_with("guard_plugin.v1")
    }

    /// Whether this ref declares the transform plugin kind
    /// (`...transform_plugin.v1~`).
    pub fn is_transform_kind(&self) -> bool {
        self.type_prefix().ends_with("transform_plugin.v1")
    }

    /// Extract the embedded plugin UUID (custom plugins) from a ref of
    /// the form `...v1~{uuid}`.
    pub fn plugin_uuid(&self) -> Option<Uuid> {
        self.plugin_ref
            .rsplit('~')
            .next()
            .and_then(|seg| Uuid::parse_str(seg).ok())
    }

    /// Parse a `plugins.items` entry (string or object form).
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::String(s) => Some(Self {
                plugin_ref: s.clone(),
                config: serde_json::Value::Null,
            }),
            serde_json::Value::Object(map) => {
                let plugin_ref = map.get("plugin_ref")?.as_str()?.to_owned();
                let config = map.get("config").cloned().unwrap_or(serde_json::Value::Null);
                Some(Self { plugin_ref, config })
            }
            _ => None,
        }
    }
}

/// Header transformation rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeaderTransforms {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaders>,
}

/// Inbound request header rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RequestHeaders {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub set: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub add: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<PassThrough>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Outbound response header rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResponseHeaders {
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub set: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub add: serde_json::Map<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Inbound header passthrough policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassThrough {
    None,
    Allowlist,
    All,
}

impl Default for PassThrough {
    fn default() -> Self {
        Self::None
    }
}

/// Sharing mode for hierarchy-visible configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants cannot override; always enforced.
    Enforce,
}

impl Default for SharingMode {
    fn default() -> Self {
        Self::Private
    }
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[serde(alias = "token_bucket")]
    TokenBucket,
    #[serde(alias = "sliding_window")]
    SlidingWindow,
}

/// Rate window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained rate (tokens per window).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SustainedRate {
    pub rate: u64,
    #[serde(default = "default_second_window")]
    pub window: RateWindow,
}

/// Burst capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BurstCapacity {
    /// Maximum burst size (bucket capacity); defaults to
    /// `sustained.rate`.
    pub capacity: u64,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    Global,
    Tenant,
    User,
    Ip,
    Route,
}

/// Rate-limit strategy when exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    Reject,
    Queue,
    Degrade,
}

/// Rate-limit configuration (upstream or route level).
#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RateLimit {
    #[serde(default = "default_private")]
    pub sharing: SharingMode,
    #[serde(default = "default_token_bucket")]
    pub algorithm: Algorithm,
    pub sustained: SustainedRate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    #[serde(default = "default_tenant_scope")]
    pub scope: RateScope,
    #[serde(default = "default_reject")]
    pub strategy: RateStrategy,
    #[serde(default = "default_cost")]
    pub cost: u64,
}

impl RateLimit {
    /// Effective tokens-per-second replenish rate.
    pub fn tps(&self) -> f64 {
        let window_secs = self.sustained.window.as_secs().max(1) as f64;
        self.sustained.rate as f64 / window_secs
    }

    /// Effective bucket capacity.
    pub fn capacity(&self) -> u64 {
        self.burst
            .map(|b| b.capacity)
            .unwrap_or(self.sustained.rate)
            .max(1)
    }
}

/// CORS configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CorsConfig {
    #[serde(default = "default_private")]
    pub sharing: SharingMode,
    pub enabled: bool,
    #[serde(default = "default_empty_vec")]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default = "default_empty_vec")]
    pub expose_headers: Vec<String>,
    #[serde(default = "default_false")]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Whether the origin is allowed (exact `*` or case-insensitive
    /// origin match, port-sensitive).
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|a| {
            a == "*" || a.eq_ignore_ascii_case(origin)
        })
    }

    /// Whether an actual (non-preflight) method is allowed.
    pub fn method_allowed(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Path-suffix handling for route matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject usage of a path suffix.
    Disabled,
    /// Append the suffix to `match.http.path`.
    Append,
}

impl Default for PathSuffixMode {
    fn default() -> Self {
        Self::Append
    }
}

/// Time-of-day bookkeeping shared by all records.
pub(crate) fn created_now() -> i64 {
    now_millis()
}

// =====================================================================
//                              Upstream record
// =====================================================================

/// Full upstream record (API response + persistence shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamRecord {
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub alias: String,
    #[serde(default = "default_empty_vec")]
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<UpstreamAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderTransforms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl UpstreamRecord {
    /// Effective plugin chain (upstream-bound plugins).
    pub fn plugin_refs(&self) -> Vec<PluginRef> {
        self.plugins
            .as_ref()
            .map(|p| p.items.iter().filter_map(PluginRef::from_json).collect())
            .unwrap_or_default()
    }

    /// Resolved-protocol accessor. HTTP protocol is `true` for the
    /// HTTP protocol identifier.
    pub fn is_http(&self) -> bool {
        self.protocol.ends_with("cf.core.oagw.http.v1")
    }
}

/// Upstream create/replace request body (server-managed fields absent).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamRequest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit alias. Hostname-based endpoints auto-derive; for
    /// IP-based or non-derivable endpoints this is required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default = "default_empty_vec")]
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<UpstreamAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderTransforms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

// =====================================================================
//                               Routes
// =====================================================================

/// Inbound match rules — exactly one of the protocol-scoped variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteMatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<RouteHttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<RouteGrpcMatch>,
}

impl RouteMatch {
    /// Validate that exactly one protocol variant is present.
    pub fn validate(&self) -> Result<(), String> {
        match (&self.http, &self.grpc) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => Err("route match must contain exactly one of http|grpc".into()),
            (None, None) => Err("route match must contain http or grpc rules".into()),
        }
    }
}

/// HTTP match rules.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteHttpMatch {
    #[serde(default = "default_methods")]
    pub methods: Vec<String>,
    pub path: String,
    #[serde(default = "default_empty_vec")]
    pub query_allowlist: Vec<String>,
    #[serde(default = "default_suffix_mode")]
    pub path_suffix_mode: PathSuffixMode,
}

impl RouteHttpMatch {
    /// Whether this match accepts the given method (case-insensitive).
    pub fn accepts_method(&self, method: &str) -> bool {
        self.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// gRPC match rules (phase 3 — no gRPC proxy code path is reachable).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteGrpcMatch {
    pub service: String,
    pub method: String,
}

/// Full route record (API response + persistence shape).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteRecord {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_empty_vec")]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

impl RouteRecord {
    pub fn plugin_refs(&self) -> Vec<PluginRef> {
        self.plugins
            .as_ref()
            .map(|p| p.items.iter().filter_map(PluginRef::from_json).collect())
            .unwrap_or_default()
    }
}

/// Route create request body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteRequest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_empty_vec")]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

/// Route replace request body — `upstream_id` is immutable and not
/// part of the update DTO.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteUpdateRequest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_empty_vec")]
    pub tags: Vec<String>,
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginList>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

// =====================================================================
//                              Plugins
// =====================================================================

/// Plugin kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    /// Corresponding GTS base-type segment.
    pub fn gts_segment(self) -> &'static str {
        match self {
            Self::Auth => "auth_plugin",
            Self::Guard => "guard_plugin",
            Self::Transform => "transform_plugin",
        }
    }
}

/// Custom (Starlark) plugin record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginRecord {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    pub created_at: i64,
    /// Plugin kind — shapes the GTS instance identifier
    /// (`gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`).
    pub kind: PluginKind,
    /// Human-readable handle.
    pub name: String,
    /// Starlark source (immutable after creation).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}

impl PluginRecord {
    /// Canonical GTS identifier for this plugin.
    pub fn gts_ref(&self) -> String {
        format!("gts.cf.core.oagw.{}_plugin.v1~{}", self.kind.gts_segment(), self.id)
    }
}

/// Plugin create request body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginCreateRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: PluginKind,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}
