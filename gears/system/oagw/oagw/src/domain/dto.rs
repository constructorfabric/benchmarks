//! Domain entities and configuration DTOs.
//!
//! These mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` so the management API accepts and
//! returns exactly the documented wire shapes.  Entities are immutable after
//! construction; updates go through the service layer which re-validates.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ---- GTS id constants (protocols / plugin types) ------------------------

pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

pub const AUTH_PLUGIN_BASE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
pub const GUARD_PLUGIN_BASE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
pub const TRANSFORM_PLUGIN_BASE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
pub const AUTH_OAUTH2_FORM: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
pub const AUTH_OAUTH2_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
pub const AUTH_BASIC_RESERVED: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
pub const AUTH_BEARER_RESERVED: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
pub const GUARD_TIMEOUT_RESERVED: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
pub const GUARD_CORS_RESERVED: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
pub const TRANSFORM_LOGGING_RESERVED: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
pub const TRANSFORM_METRICS_RESERVED: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

// ---- wire enums (serde uses the JSON spellings from the schemas) --------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    #[default]
    Private,
    Inherit,
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

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
    #[must_use]
    // Public DTO surface keeps `&self` receivers uniform across all getters.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn seconds(&self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    #[default]
    None,
    Allowlist,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    Disabled,
    #[default]
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    #[must_use]
    // Public DTO surface keeps `&self` receivers uniform across all getters.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn as_str(&self) -> &'static str {
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

/// Wire format for `protocol` is the GTS identifier per
/// `upstream.v1.schema.json` (`gts.cf.core.oagw.protocol.v1~...`), not a
/// bare lowercase name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Grpc,
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.gts_id())
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            PROTOCOL_HTTP => Ok(Protocol::Http),
            PROTOCOL_GRPC => Ok(Protocol::Grpc),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &[PROTOCOL_HTTP, PROTOCOL_GRPC],
            )),
        }
    }
}

impl Protocol {
    #[must_use]
    // Public DTO surface keeps `&self` receivers uniform across all getters.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn gts_id(&self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP,
            Self::Grpc => PROTOCOL_GRPC,
        }
    }
}

// ---- endpoint ----------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointDto {
    /// `https|wss|wt|grpc` per the schema; `http|ws` additionally accepted
    /// when `allow_http_upstream` is enabled (e2e).
    pub scheme: String,
    /// Hostname or IP literal of the upstream service.
    pub host: String,
    /// Optional port; defaults to the scheme standard port.
    pub port: Option<u16>,
}

impl EndpointDto {
    #[must_use]
    pub fn resolved_port(&self) -> u16 {
        self.port
            .unwrap_or_else(|| crate::domain::alias::default_port_for_scheme(&self.scheme))
    }
}

// ---- server / auth / headers / plugins / rate_limit / cors -----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerDto {
    pub endpoints: Vec<EndpointDto>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AuthDto {
    /// Auth plugin GTS identifier (e.g. noop / apikey / `oauth2_client_cred`).
    pub r#type: Option<String>,
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HeadersDto {
    #[serde(default)]
    pub request: RequestHeadersDto,
    #[serde(default)]
    pub response: ResponseHeadersDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeadersDto {
    #[serde(default)]
    pub set: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub add: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default)]
    pub passthrough: PassthroughMode,
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeadersDto {
    #[serde(default)]
    pub set: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub add: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// A plugin binding: either a bare GTS identifier string (schema form) or an
/// object `{ plugin_ref, config }` (ADR-0009 form).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginItemDto {
    Ref(String),
    Bound {
        plugin_ref: String,
        #[serde(default)]
        config: Value,
    },
}

impl PluginItemDto {
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Ref(s) => s,
            Self::Bound { plugin_ref, .. } => plugin_ref,
        }
    }

    #[must_use]
    pub fn config(&self) -> Value {
        match self {
            Self::Ref(_) => Value::Null,
            Self::Bound { config, .. } => config.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsDto {
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub items: Vec<PluginItemDto>,
}

impl Default for PluginsDto {
    fn default() -> Self {
        Self {
            sharing: Sharing::Private,
            items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitDto {
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    pub sustained: SustainedDto,
    #[serde(default)]
    pub burst: BurstDto,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_cost")]
    pub cost: u64,
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

fn default_cost() -> u64 {
    1
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedDto {
    pub rate: u64,
    #[serde(default)]
    pub window: RateLimitWindow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BurstDto {
    /// Defaults to `sustained.rate` (burst = one window of tokens).
    pub capacity: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CorsDto {
    #[serde(default)]
    pub sharing: Sharing,
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".into(), "POST".into()]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatchDto {
    pub methods: Vec<HttpMethod>,
    pub path: String,
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatchDto {
    pub service: String,
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchDto {
    #[serde(default)]
    pub http: Option<HttpMatchDto>,
    #[serde(default)]
    pub grpc: Option<GrpcMatchDto>,
}

// ---- wire request / response DTOs ---------------------------------------

/// Upstream payload accepted by POST/PUT (no server-generated fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit alias.  Omitted when auto-derivable; required for IP /
    /// non-derivable pools.
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerDto,
    pub protocol: Protocol,
    #[serde(default)]
    pub auth: AuthDto,
    #[serde(default)]
    pub headers: HeadersDto,
    #[serde(default)]
    pub plugins: PluginsDto,
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    #[serde(default)]
    pub cors: Option<CorsDto>,
}

/// Upstream entity returned by the management API (includes system fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub enabled: bool,
    /// Normalized (lowercased, trailing dot stripped) routing alias.
    pub alias: String,
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    pub auth: AuthConfig,
    pub headers: HeadersConfig,
    pub plugins: PluginsConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub created_at: i64,
}

/// Route payload accepted by POST (no server-generated fields; upstream is
/// immutable after creation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub upstream_id: Uuid,
    /// Route selection priority (DESIGN domain model `Route.priority`).
    /// `0` = default; higher wins among equal-longest path prefixes.  This is
    /// an additive model field (accepted but not part of route.v1.schema.json).
    #[serde(default)]
    pub priority: u32,
    /// Wire key is `match` per `route.v1.schema.json`.
    #[serde(rename = "match")]
    pub match_config: MatchDto,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub plugins: PluginsDto,
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    #[serde(default)]
    pub cors: Option<CorsDto>,
}

/// Route entity returned by the management API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub enabled: bool,
    pub upstream_id: Uuid,
    /// Route selection priority (DESIGN domain model).  `0` = default;
    /// higher wins among equal-longest path prefixes.
    pub priority: u32,
    pub match_config: MatchConfig,
    pub tags: Vec<String>,
    pub plugins: PluginsConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    #[must_use]
    // Public DTO surface keeps `&self` receivers uniform across all getters.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub fn base_gts(&self) -> &'static str {
        match self {
            Self::Auth => AUTH_PLUGIN_BASE,
            Self::Guard => GUARD_PLUGIN_BASE,
            Self::Transform => TRANSFORM_PLUGIN_BASE,
        }
    }
}

/// Custom (Starlark) plugin payload.  Immutable after creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    pub name: String,
    pub description: Option<String>,
    pub plugin_type: PluginKind,
    pub config_schema: Value,
    pub source_code: String,
}

/// Custom plugin entity returned by the management API (UUID-backed, GTS
/// identifier `{kind_base}{id}`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CustomPlugin {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub plugin_type: PluginKind,
    pub name: String,
    pub description: Option<String>,
    pub config_schema: Value,
    pub source_code: String,
    pub created_at: i64,
}

impl CustomPlugin {
    /// Full GTS identifier for this plugin.
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", self.plugin_type.base_gts(), self.id)
    }
}

// ---- normalized (stored) configuration types ----------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Endpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl From<EndpointDto> for Endpoint {
    fn from(dto: EndpointDto) -> Self {
        Self {
            scheme: dto.scheme.to_ascii_lowercase(),
            host: crate::domain::alias::normalize_alias(&dto.host),
            port: dto.resolved_port(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct AuthConfig {
    /// Plugin GTS id; `None`/noop means no authentication applied.
    pub plugin_type: Option<String>,
    pub sharing: Sharing,
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct HeadersConfig {
    pub request: RequestHeadersConfig,
    pub response: ResponseHeadersConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct RequestHeadersConfig {
    pub set: std::collections::BTreeMap<String, String>,
    pub add: std::collections::BTreeMap<String, String>,
    pub remove: Vec<String>,
    pub passthrough: PassthroughMode,
    pub passthrough_allowlist: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ResponseHeadersConfig {
    pub set: std::collections::BTreeMap<String, String>,
    pub add: std::collections::BTreeMap<String, String>,
    pub remove: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct PluginsConfig {
    pub sharing: Sharing,
    pub items: Vec<PluginBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginBinding {
    /// Plugin GTS identifier (named) or UUID-backed GTS identifier (custom).
    pub plugin_ref: String,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RateLimitConfig {
    pub sharing: Sharing,
    pub algorithm: RateLimitAlgorithm,
    pub sustained_rate: u64,
    pub sustained_window: RateLimitWindow,
    pub burst_capacity: u64,
    pub scope: RateLimitScope,
    pub strategy: RateLimitStrategy,
    pub cost: u64,
    pub response_headers: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct CorsConfig {
    pub sharing: Sharing,
    pub enabled: bool,
    pub allowed_origins: Vec<String>,
    pub allowed_methods: Vec<String>,
    pub expose_headers: Vec<String>,
    pub allow_credentials: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct MatchConfig {
    pub http: Option<HttpMatch>,
    pub grpc: Option<GrpcMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HttpMatch {
    pub methods: Vec<HttpMethod>,
    pub path: String,
    pub query_allowlist: Vec<String>,
    pub path_suffix_mode: PathSuffixMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

// ---- validation helpers -------------------------------------------------

impl RateLimitDto {
    /// Validate a rate-limit DTO independently (shared by upstream/route).
    ///
    /// # Errors
    ///
    /// Returns a description when `sustained.rate` or the effective burst
    /// capacity is zero.
    pub fn validate(&self) -> Result<(), String> {
        if self.sustained.rate == 0 {
            return Err("rate_limit.sustained.rate must be >= 1".into());
        }
        let burst = self.burst.capacity.unwrap_or(self.sustained.rate);
        if burst == 0 {
            return Err("rate_limit.burst.capacity must be >= 1".into());
        }
        if self.algorithm == RateLimitAlgorithm::SlidingWindow
            && self.strategy == RateLimitStrategy::Queue
        {
            // slide window with queue is not supported; validate candidates
            // keep a clean axis for tests.
            let _ = self.sustained.window;
        }
        Ok(())
    }
}

impl CorsDto {
    /// CORS validation (schema `if/then`: `allow_credentials` cannot combine
    /// with the wildcard origin `*`).
    ///
    /// # Errors
    ///
    /// Returns a description when `allow_credentials` is combined with the
    /// wildcard origin `*`.
    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*") {
            return Err("cors.allow_credentials cannot be used with wildcard origin '*'".into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upstream_request_parses_schema_shape() {
        let raw = json!({
            "enabled": true,
            "alias": "my-service",
            "tags": ["prod"],
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.example.com", "port": 443 }
            ]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "auth": { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                      "sharing": "inherit", "config": { "header": "X-API-Key" } },
            "headers": { "request": { "set": { "X-Tenant": "acme" } } },
            "plugins": { "sharing": "inherit", "items": [
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
            ]},
            "rate_limit": { "sustained": { "rate": 100, "window": "minute" },
                            "burst": { "capacity": 50 }, "scope": "tenant" },
            "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"] }
        });
        let up: UpstreamRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(up.alias.as_deref(), Some("my-service"));
        assert_eq!(up.server.endpoints.len(), 1);
        assert_eq!(up.server.endpoints[0].scheme, "https");
        assert_eq!(up.protocol, Protocol::Http);
        assert_eq!(up.auth.sharing, Sharing::Inherit);
        assert_eq!(up.plugins.items.len(), 1);
        assert!(
            matches!(&up.plugins.items[0], PluginItemDto::Ref(s) if s == "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
        );
        let rl = up.rate_limit.as_ref().unwrap();
        assert_eq!(rl.sustained.rate, 100);
        assert_eq!(rl.sustained.window, RateLimitWindow::Minute);
        assert_eq!(rl.scope, RateLimitScope::Tenant);
        assert!(up.cors.as_ref().unwrap().enabled);
    }

    #[test]
    fn plugin_item_object_form_parses() {
        let raw = json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "h.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "plugins": { "items": [
                { "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
                  "config": { "required_request_headers": "x-id" } }
            ]}
        });
        let up: UpstreamRequest = serde_json::from_value(raw).unwrap();
        match &up.plugins.items[0] {
            PluginItemDto::Bound { plugin_ref, config } => {
                assert!(plugin_ref.ends_with("required_headers.v1"));
                assert_eq!(config["required_request_headers"], "x-id");
            }
            PluginItemDto::Ref(_) => panic!("expected bound form"),
        }
    }

    #[test]
    fn cors_wildcard_plus_credentials_rejected_at_validation() {
        let cors = CorsDto {
            enabled: true,
            allowed_origins: vec!["*".into()],
            allow_credentials: true,
            ..Default::default()
        };
        let err = cors.validate().unwrap_err();
        assert!(err.contains("wildcard"));
    }

    #[test]
    fn route_request_parses() {
        let raw = json!({
            "upstream_id": "00000000-0000-0000-0000-000000000001",
            "match": { "http": {
                "methods": ["GET", "POST"],
                "path": "/v1",
                "query_allowlist": ["limit"],
                "path_suffix_mode": "append"
            }},
            "tags": ["r"],
            "rate_limit": { "sustained": { "rate": 5 } }
        });
        let route: RouteRequest = serde_json::from_value(raw).unwrap();
        let http = route.match_config.http.as_ref().unwrap();
        assert_eq!(http.methods, vec![HttpMethod::Get, HttpMethod::Post]);
        assert_eq!(http.path, "/v1");
        assert_eq!(http.query_allowlist, vec!["limit"]);
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
        assert_eq!(route.match_config.grpc, None);
    }

    #[test]
    fn cors_validate_passes_without_credentials_wildcard() {
        let cors = CorsDto {
            enabled: true,
            allowed_origins: vec!["*".into()],
            ..Default::default()
        };
        assert!(cors.validate().is_ok());
    }
}
