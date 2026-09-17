//! OAGW domain model.
//!
//! Wire shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`. Protocol / plugin identifiers are
//! full GTS identifiers; see `DESIGN.md §3.1`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// GTS identifiers
// ---------------------------------------------------------------------------

/// GTS fragment for the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS fragment for the gRPC upstream protocol.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`
pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`
pub const AUTH_OAUTH2_CC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1`
pub const AUTH_OAUTH2_CC_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// Catalog-only `basic.v1` identifier (no backing `AuthPlugin`).
pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only `bearer.v1` identifier (no backing `AuthPlugin`).
pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`
pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only `timeout.v1` guard identifier (core DP functionality).
pub const GUARD_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalog-only `cors.v1` guard identifier (core DP functionality).
pub const GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// `gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1`
pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only `logging.v1` transform identifier (core DP instrumentation).
pub const TRANSFORM_LOGGING: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only `metrics.v1` transform identifier (core DP instrumentation).
pub const TRANSFORM_METRICS: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Whether `id` is a resolvable named built-in plugin identifier.
#[must_use]
pub fn is_builtin_auth_plugin(id: &str) -> bool {
    matches!(
        id,
        AUTH_NOOP | AUTH_APIKEY | AUTH_OAUTH2_CC | AUTH_OAUTH2_CC_BASIC
    )
}

/// Whether `id` is a resolvable named built-in guard plugin identifier.
#[must_use]
pub fn is_builtin_guard_plugin(id: &str) -> bool {
    matches!(id, GUARD_REQUIRED_HEADERS)
}

/// Whether `id` is a resolvable named built-in transform plugin identifier.
#[must_use]
pub fn is_builtin_transform_plugin(id: &str) -> bool {
    matches!(id, TRANSFORM_REQUEST_ID)
}

// ---------------------------------------------------------------------------
// Sharing modes
// ---------------------------------------------------------------------------

/// Hierarchical sharing mode (`private` / `inherit` / `enforce`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants can override.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

impl SharingMode {
    /// `true` when this configuration is inherited by descendants.
    #[must_use]
    pub fn is_inherited(self) -> bool {
        matches!(self, Self::Inherit | Self::Enforce)
    }
}

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// `https`, `wss`, `wt`, `grpc` — or `http` when plaintext upstreams are
    /// allowed by gear configuration.
    pub scheme: String,
    /// Hostname or IP literal (RFC 1123 validated).
    pub host: String,
    /// TCP port; defaults to 443 when omitted.
    #[serde(default)]
    pub port: u16,
}

impl Endpoint {
    /// `true` when the endpoint is an IPv4 / IPv6 literal.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }

    /// Standard port for this scheme (80 / 443), omitted from derived aliases.
    #[must_use]
    pub fn is_standard_port(&self) -> bool {
        matches!(self.scheme.as_str(), "https" | "wss" | "wt" | "grpc") && self.port == 443
            || matches!(self.scheme.as_str(), "http") && self.port == 80
    }

    /// `true` for schemes that normally run over plaintext HTTP.
    #[must_use]
    pub fn is_plaintext(&self) -> bool {
        matches!(self.scheme.as_str(), "http")
    }

    /// `true` for WebSocket-style schemes that still speak HTTP/1.1 framing.
    #[must_use]
    pub fn is_websocket_family(&self) -> bool {
        matches!(self.scheme.as_str(), "wss" | "ws")
    }
}

/// Server endpoint pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Pool members. All must share protocol, scheme and port.
    pub endpoints: Vec<Endpoint>,
}

// ---------------------------------------------------------------------------
// Header rules
// ---------------------------------------------------------------------------

/// Response-side header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to set (overwrite if present).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the upstream response.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if present).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded upstream.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough == allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Which inbound request headers are forwarded upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward no inbound headers (default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header (minus routing and hop-by-hop headers).
    All,
}

/// Header transformation rules for a resource.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeaderRules {
    /// Inbound → outbound rules.
    #[serde(default)]
    pub request: RequestHeaderRules,
    /// Upstream response → client response rules.
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// Token-bucket time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    /// One second.
    #[default]
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
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained rate component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window length (default `second`).
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Burst {
    /// Bucket capacity; defaults to `sustained.rate`.
    pub capacity: u64,
}

/// Counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateScope {
    /// One global counter.
    Global,
    /// Per-tenant counter (default).
    #[default]
    Tenant,
    /// Per-authenticated-user counter.
    User,
    /// Per-client-IP counter.
    Ip,
    /// Per-route counter.
    Route,
}

/// Overflow strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Queue within bounded capacity.
    Queue,
    /// Forward with reduced functionality.
    Degrade,
}

/// Token-bucket rate limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// `token_bucket` (default) or `sliding_window`.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate — required.
    pub sustained: SustainedRate,
    /// Burst capacity.
    #[serde(default)]
    pub burst: Option<Burst>,
    /// Counter scope (default `tenant`).
    #[serde(default)]
    pub scope: RateScope,
    /// Overflow strategy (default `reject`).
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request (default 1).
    #[serde(default = "default_cost")]
    pub cost: u64,
}

/// Rate limiting algorithm selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket — allows bursts (default).
    #[default]
    TokenBucket,
    /// Sliding window — prevents boundary bursts.
    SlidingWindow,
}

fn default_cost() -> u64 {
    1
}

impl RateLimit {
    /// Bucket capacity: `burst.capacity` or `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst.as_ref().map_or(self.sustained.rate, |b| b.capacity)
    }

    /// Refill interval between tokens.
    #[must_use]
    pub fn refill_interval(&self) -> Duration {
        let rate = self.sustained.rate.max(1);
        Duration::from_millis((self.sustained.window.seconds() * 1000) / rate)
    }

    /// `Retry-After` hint in seconds derived from the refill cadence.
    #[must_use]
    pub fn retry_after_seconds(&self) -> u64 {
        self.refill_interval().as_secs().clamp(1, 3600)
    }
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

/// Per-resource CORS configuration. See `ADR/0004-cors.md`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS processing is enabled for this resource.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins: `["*"]` or explicit scheme+host origins.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods (default `GET`, `POST`).
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond CORS-safelisted ones.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Allow credentialed requests; incompatible with `["*"]`.
    #[serde(default)]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// `true` when the configured origin set contains `*`.
    #[must_use]
    pub fn allows_wildcard(&self) -> bool {
        self.allowed_origins.iter().any(|o| o == "*")
    }

    /// `true` when `origin` is allowed by this configuration.
    #[must_use]
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        self.allows_wildcard()
            || self
                .allowed_origins
                .iter()
                .any(|o| o.eq_ignore_ascii_case(origin))
    }

    /// `true` when `method` is allowed by this configuration.
    #[must_use]
    pub fn is_method_allowed(&self, method: &http::Method) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    }
}

// ---------------------------------------------------------------------------
// Auth config
// ---------------------------------------------------------------------------

/// Upstream outbound-authentication configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// Auth plugin identifier (GTS id or UUID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Alias accepted by the JSON schema: `auth.type`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration, including `secret_ref`.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl AuthConfig {
    /// Effective plugin identifier (`type` takes precedence).
    #[must_use]
    pub fn plugin_id(&self) -> Option<&str> {
        self.r#type
            .as_deref()
            .or(self.plugin_type.as_deref())
            .filter(|s| !s.is_empty())
    }

    /// Read a string-valued key from `config`.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(|v| v.as_str())
    }

    /// Read an unsigned integer from `config`.
    #[must_use]
    pub fn config_u64(&self, key: &str) -> Option<u64> {
        self.config.get(key).and_then(serde_json::Value::as_u64)
    }
}

// ---------------------------------------------------------------------------
// Plugin bindings
// ---------------------------------------------------------------------------

/// A single plugin binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginBinding {
    /// Canonical plugin identifier (GTS id or UUID string).
    pub plugin_ref: String,
    /// Extracted UUID when `plugin_ref` is UUID-backed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// Plugin configuration (e.g. `required_request_headers`).
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// Plugin chain configuration for a resource.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered plugin bindings.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

impl PluginsConfig {
    /// Parse an item from its wire form: either a bare string or an object.
    #[must_use]
    pub fn parse_item(value: &serde_json::Value) -> Option<PluginBinding> {
        match value {
            serde_json::Value::String(s) => Some(PluginBinding {
                plugin_ref: s.clone(),
                plugin_uuid: parse_uuid_suffix(s),
                config: BTreeMap::new(),
            }),
            serde_json::Value::Object(map) => {
                let plugin_ref = map
                    .get("plugin_ref")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| map.get("id").and_then(serde_json::Value::as_str))
                    .map(str::to_owned)?;
                Some(PluginBinding {
                    plugin_uuid: map
                        .get("plugin_uuid")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|s| Uuid::parse_str(s).ok())
                        .or_else(|| parse_uuid_suffix(&plugin_ref)),
                    config: map
                        .get("config")
                        .and_then(serde_json::Value::as_object)
                        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                        .unwrap_or_default(),
                    plugin_ref,
                })
            }
            _ => None,
        }
    }
}

/// Extract a trailing UUID from a GTS identifier or bare UUID string.
#[must_use]
pub fn parse_uuid_suffix(id: &str) -> Option<Uuid> {
    Uuid::parse_str(id).ok().or_else(|| {
        id.rsplit('~').next().and_then(|tail| Uuid::parse_str(tail).ok())
    })
}

// ---------------------------------------------------------------------------
// Resources
// ---------------------------------------------------------------------------

/// Upstream service configuration (tenant-scoped root object).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing identifier (lowercase, unique per tenant).
    pub alias: String,
    /// Whether the upstream accepts traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol GTS identifier.
    pub protocol: String,
    /// Flat tags (additive across the hierarchy).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeaderRules,
    /// Upstream-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// Upstream-level CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Outbound auth configuration.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Guard / transform plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last modification timestamp (RFC 3339).
    pub updated_at: String,
}

fn default_true() -> bool {
    true
}

/// A route attached to an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Referenced upstream (immutable after create).
    pub upstream_id: Uuid,
    /// Flat tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Protocol-scoped match rule.
    pub r#match: MatchRule,
    /// Route-level plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Route-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// Route-level CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Route-level header rules (merged over the upstream's).
    #[serde(default)]
    pub headers: Option<HeaderRules>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last modification timestamp (RFC 3339).
    pub updated_at: String,
}

/// Protocol-scoped inbound match rule. Exactly one variant is set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchRule {
    /// HTTP method + path matching.
    Http(HttpMatch),
    /// gRPC `(service, method)` matching (Phase 3 — validation only).
    Grpc(GrpcMatch),
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Allowed methods.
    pub methods: Vec<String>,
    /// Path prefix pattern.
    pub path: String,
    /// Allowed query parameters; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` from the proxy URL is treated (default `append`).
    #[serde(default = "default_suffix_mode")]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully-qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Default for `HttpMatch::path_suffix_mode`.
fn default_suffix_mode() -> PathSuffixMode {
    PathSuffixMode::Append
}

/// How the proxy path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject requests that carry a path suffix.
    Disabled,
    /// Append the suffix to `path` (default).
    #[default]
    Append,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: vec!["GET".to_owned()],
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::default(),
        }
    }
}

/// A tenant-defined custom plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Plugin {
    /// System-generated UUID.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// `auth_plugin` / `guard_plugin` / `transform_plugin`.
    pub plugin_type: String,
    /// Human-readable name.
    pub name: String,
    /// Configuration schema (JSON Schema object).
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    /// Plugin source code.
    #[serde(default)]
    pub source_code: String,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last-use timestamp, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// GC eligibility timestamp, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<String>,
}

/// Plugin type discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    /// Outbound authentication.
    Auth,
    /// Request / response guard.
    Guard,
    /// Header / body transformation.
    Transform,
}

impl PluginKind {
    /// GTS base-type fragment for this plugin kind.
    #[must_use]
    pub fn gts_fragment(self) -> &'static str {
        match self {
            Self::Auth => "auth_plugin",
            Self::Guard => "guard_plugin",
            Self::Transform => "transform_plugin",
        }
    }

    /// Classify a GTS plugin identifier, if recognized.
    #[must_use]
    pub fn from_ref(plugin_ref: &str) -> Option<Self> {
        for (needle, kind) in [
            (".oagw.auth_plugin.v1~", Self::Auth),
            (".oagw.guard_plugin.v1~", Self::Guard),
            (".oagw.transform_plugin.v1~", Self::Transform),
        ] {
            if plugin_ref.contains(needle) {
                return Some(kind);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_classifies_ips() {
        let e = Endpoint {
            scheme: "https".to_owned(),
            host: "10.0.1.1".to_owned(),
            port: 443,
        };
        assert!(e.is_ip());
        assert!(e.is_standard_port());
    }

    #[test]
    fn auth_config_prefers_type() {
        let cfg = AuthConfig {
            plugin_type: Some("a".to_owned()),
            r#type: Some(AUTH_APIKEY.to_owned()),
            ..AuthConfig::default()
        };
        assert_eq!(cfg.plugin_id(), Some(AUTH_APIKEY));
    }

    #[test]
    fn parse_uuid_suffix_handles_both_forms() {
        let u = Uuid::new_v4();
        assert_eq!(parse_uuid_suffix(&u.to_string()), Some(u));
        assert_eq!(
            parse_uuid_suffix(&format!("gts.cf.core.x.v1~{u}")),
            Some(u)
        );
        assert_eq!(parse_uuid_suffix("cf.core.oagw.apikey.v1"), None);
    }

    #[test]
    fn cors_method_matching_is_case_insensitive() {
        let cors = CorsConfig {
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            ..Default::default()
        };
        assert!(cors.is_method_allowed(&http::Method::GET));
        assert!(!cors.is_method_allowed(&http::Method::DELETE));
    }

    #[test]
    fn rate_limit_capacity_defaults_to_sustained() {
        let rl = RateLimit {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: SustainedRate {
                rate: 5,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
        };
        assert_eq!(rl.capacity(), 5);
    }
}
