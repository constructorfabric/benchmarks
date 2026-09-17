//! Domain model for the OAGW gear: upstreams, routes, custom plugins and
//! their configuration blocks.
//!
//! The structs mirror the wire schemas (`docs/schemas/upstream.v1.schema.json`,
//! `docs/schemas/route.v1.schema.json`) so serde round-trips are 1:1.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Tenant identifier.
pub type TenantId = Uuid;

/// Protocol spoken by an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Http,
    Grpc,
}

impl Default for Protocol {
    fn default() -> Self {
        Self::Http
    }
}

impl Protocol {
    /// Standard (alias-omitted) port for the protocol.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Grpc => 443,
        }
    }

    #[must_use]
    pub const fn is_standard_port(self, port: u16) -> bool {
        self.standard_port() == port
    }

    #[must_use]
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            "http" => Some(Self::Http),
            "https" | "grpc" | "grpcs" | "ws" | "wss" | "wt" => Some(Self::Grpc),
            _ => None,
        }
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// URL scheme (`http`, `https`, `grpc`, `grpcs`, `ws`, `wss`, `wt`).
    pub scheme: String,
    /// Hostname or IP literal (trailing dot tolerated and stripped on
    /// hostnames).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl Endpoint {
    /// Whether `host` is an IP address literal.
    #[must_use]
    pub fn host_is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }

    /// `scheme://host:port` for diagnostics.
    #[must_use]
    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Whether requests to this endpoint must use TLS.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        matches!(
            self.scheme.as_str(),
            "https" | "grpcs" | "wss" | "wt"
        )
    }
}

/// Server configuration for an upstream (endpoint pool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ServerConfig {
    pub endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// Validate endpoints: non-empty, consistent scheme + port, valid host.
    ///
    /// # Errors
    ///
    /// `DomainError::Validation` with a human-readable message.
    pub fn validate(&self, protocol: Protocol) -> Result<(), DomainError> {
        if self.endpoints.is_empty() {
            return Err(DomainError::validation("server.endpoints must not be empty"));
        }
        let first = &self.endpoints[0];
        for ep in &self.endpoints {
            if ep.scheme != first.scheme {
                return Err(DomainError::validation(format!(
                    "all endpoints must share the same scheme (got '{}' and '{}')",
                    first.scheme, ep.scheme
                )));
            }
            if ep.port != first.port {
                return Err(DomainError::validation(format!(
                    "all endpoints must share the same port (got {} and {})",
                    first.port, ep.port
                )));
            }
            if !crate::domain::alias::is_valid_hostname(&ep.host) {
                return Err(DomainError::validation(format!(
                    "invalid endpoint host '{}'",
                    ep.host
                )));
            }
            if protocol == Protocol::Grpc && ep.scheme != "grpc" && ep.scheme != "grpcs" {
                return Err(DomainError::validation(
                    "gRPC upstreams require a grpc/grpcs endpoint scheme",
                ));
            }
        }
        Ok(())
    }
}

/// Sharing mode for hierarchical configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; descendant may override.
    Inherit,
    /// Visible; descendant cannot override.
    Enforce,
}

/// Authentication configuration on an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    /// GTS type identifier of the auth plugin (e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`).
    /// Wire key is `type` per `docs/schemas/upstream.v1.schema.json`.
    #[serde(default, rename = "type")]
    pub auth_type: String,
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration object.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Passthrough strategy for inbound request headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward no inbound headers.
    #[default]
    None,
    /// Forward only the listed headers.
    Allowlist,
    /// Forward all except hop-by-hop headers.
    All,
}

/// Header transformation rules for one direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HeaderRules {
    /// Headers to set (overwrite if present).
    #[serde(default)]
    pub set: Vec<(String, String)>,
    /// Headers to add (append, allow duplicates).
    #[serde(default)]
    pub add: Vec<(String, String)>,
    /// Header names to remove.
    #[serde(default)]
    pub remove: Vec<String>,
    #[serde(default)]
    pub passthrough: PassthroughMode,
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Header configuration for an upstream (request + response).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: HeaderRules,
    #[serde(default)]
    pub response: HeaderRules,
}

/// A plugin binding inside an upstream/route plugin list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginBinding {
    /// Referenced plugin identifier.
    ///
    /// For named (built-in/catalog) plugins: the GTS plugin type id, e.g.
    /// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`.
    /// For custom plugins: the anonymous GTS instance id
    /// `gts.cf.core.oagw.guard_plugin.v1~{uuid}`.
    pub plugin_ref: String,
    /// Resolved custom plugin row id (when `plugin_ref` points at a custom
    /// plugin).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_uuid: Option<Uuid>,
    /// 0-based position within the owning plugin list (assigned on save).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// Per-binding configuration object.
    #[serde(default)]
    pub config: serde_json::Value,
}

impl PluginBinding {
    /// The plugin kind token (`auth` / `guard` / `transform`) parsed from the
    /// GTS type segment of `plugin_ref`, if it carries one.
    #[must_use]
    pub fn kind(&self) -> Option<PluginKind> {
        PluginKind::from_gts_id(&self.plugin_ref)
    }
}

/// Plugin list configuration attached to an upstream or route.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// The three plugin categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl std::fmt::Display for PluginKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        })
    }
}

impl PluginKind {
    /// Base type id for a kind.
    #[must_use]
    pub fn base_type(self) -> &'static str {
        match self {
            Self::Auth => crate::domain::gts::AUTH_PLUGIN_TYPE,
            Self::Guard => crate::domain::gts::GUARD_PLUGIN_TYPE,
            Self::Transform => crate::domain::gts::TRANSFORM_PLUGIN_TYPE,
        }
    }

    /// Kind inferred from a full plugin GTS id, e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` → `Auth`.
    #[must_use]
    pub fn from_gts_id(id: &str) -> Option<Self> {
        if id.starts_with(crate::domain::gts::AUTH_PLUGIN_TYPE) {
            Some(Self::Auth)
        } else if id.starts_with(crate::domain::gts::GUARD_PLUGIN_TYPE) {
            Some(Self::Guard)
        } else if id.starts_with(crate::domain::gts::TRANSFORM_PLUGIN_TYPE) {
            Some(Self::Transform)
        } else {
            None
        }
    }

    /// Single-kind sub-selection over bindings (preserving order).
    #[must_use]
    pub fn select<'a>(
        &self,
        bindings: impl IntoIterator<Item = &'a PluginBinding>,
    ) -> Vec<&'a PluginBinding> {
        bindings
            .into_iter()
            .filter(|b| b.kind() == Some(*self))
            .collect()
    }
}

/// Rate-limit time window token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateWindow {
    Second,
    Minute,
    Hour,
    Day,
}

impl Default for RateWindow {
    fn default() -> Self {
        Self::Second
    }
}

impl RateWindow {
    /// Length of the window in seconds.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Sustained rate component of a dual-rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedLimit {
    /// Tokens replenished per `window`.
    pub rate: u64,
    #[serde(default)]
    pub window: RateWindow,
}

impl Default for SustainedLimit {
    fn default() -> Self {
        Self {
            rate: 1,
            window: RateWindow::Second,
        }
    }
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    #[default]
    TokenBucket,
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    #[default]
    Global,
    Tenant,
    User,
    Ip,
    Route,
}

/// Behavior when a limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Dual-rate rate-limit configuration (ADR 0006).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    #[serde(default)]
    pub sustained: SustainedLimit,
    /// Maximum burst capacity (bucket size). Defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u64>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    #[serde(default = "default_cost")]
    pub cost: u32,
}

const fn default_cost() -> u32 {
    1
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateLimitAlgorithm::default(),
            sustained: SustainedLimit::default(),
            burst: None,
            scope: RateLimitScope::default(),
            strategy: RateLimitStrategy::default(),
            cost: 1,
        }
    }
}

impl RateLimitConfig {
    /// Effective bucket capacity (`burst.capacity` or `sustained.rate`).
    #[must_use]
    pub fn capacity(&self) -> f64 {
        self.burst.unwrap_or(self.sustained.rate) as f64
    }

    /// Refill rate in tokens/second from the sustained window.
    #[must_use]
    pub fn refill_per_sec(&self) -> f64 {
        let window = self.sustained.window.as_secs().max(1);
        self.sustained.rate as f64 / window as f64
    }
}

/// CORS configuration (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: SharingMode,
    #[serde(default)]
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
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_allowed_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl CorsConfig {
    /// Validate the CORS block.
    ///
    /// # Errors
    ///
    /// `DomainError::Validation` when `allow_credentials` is combined with a
    /// wildcard origin, or `allowed_origins` mixes a wildcard with explicit
    /// origins.
    pub fn validate(&self) -> Result<(), DomainError> {
        let has_wildcard = self.allowed_origins.iter().any(|o| o == "*");
        let has_explicit = self.allowed_origins.iter().any(|o| o != "*");
        if self.allow_credentials && has_wildcard {
            return Err(DomainError::validation(
                "cors.allow_credentials=true requires specific origins (not '*')",
            ));
        }
        if has_wildcard && has_explicit {
            return Err(DomainError::validation(
                "cors.allowed_origins cannot mix '*' with explicit origins",
            ));
        }
        Ok(())
    }
}

/// An upstream resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub enabled: bool,
    /// Routing alias. `None` until auto-derived or explicitly assigned.
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub headers: HeadersConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Created-through-binding indicator: when set, this upstream shadows an
    /// ancestor resource rather than defining a brand-new alias.
    #[serde(default, skip_serializing)]
    pub bound: bool,
}

/// HTTP match rules for a route.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HttpMatch {
    /// Allowed HTTP methods (empty = all).
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path prefix pattern, starting with `/`.
    pub path: String,
    /// Query-parameter key allowlist (empty = allow none).
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Whether a trailing `/path_suffix` from the proxy URL is appended to
    /// `path` (`append`) or rejected (`disabled`).
    #[serde(default = "default_path_suffix_mode")]
    pub path_suffix_mode: PathSuffixMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    #[default]
    Disabled,
    Append,
}

fn default_path_suffix_mode() -> PathSuffixMode {
    PathSuffixMode::Append
}

/// gRPC match rules (planned — Phase 3, no proxy code path yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Route match rules: exactly one of `http` / `grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            http: None,
            grpc: None,
        }
    }
}

impl MatchConfig {
    /// Validate that exactly one of `http`/`grpc` is set.
    ///
    /// # Errors
    ///
    /// `DomainError::Validation` otherwise.
    pub fn validate(&self) -> Result<(), DomainError> {
        match (&self.http, &self.grpc) {
            (Some(_), Some(_)) => Err(DomainError::validation(
                "route.match must specify exactly one of http or grpc",
            )),
            (None, None) => Err(DomainError::validation(
                "route.match must specify exactly one of http or grpc",
            )),
            (Some(m), None) => {
                if m.path.is_empty() || !m.path.starts_with('/') {
                    return Err(DomainError::validation(
                        "route.match.http.path must start with '/'",
                    ));
                }
                Ok(())
            }
            (None, Some(_)) => Err(DomainError::validation(
                "gRPC routes are not yet supported; use match.http",
            )),
        }
    }
}

/// A route resource bound to an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    pub id: Uuid,
    pub tenant_id: TenantId,
    /// Owning upstream (immutable after creation).
    pub upstream_id: Uuid,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Route priority; higher priority wins when path prefix length ties.
    #[serde(default)]
    pub priority: i32,
    /// Wire key is `match` per `docs/schemas/route.v1.schema.json`.
    #[serde(default, rename = "match")]
    pub match_: MatchConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

const fn default_true() -> bool {
    true
}

/// A custom (Starlark) plugin resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: PluginKind,
    /// Config JSON-Schema for bindings referencing this plugin.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Starlark source (sandboxed execution).
    pub source_code: String,
    /// Garbage-collection eligibility marker for unlinked plugins
    /// (`Some(timestamp)` when unlinked and TTL elapsed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<u64>,
}

impl Plugin {
    /// Immutable GTS instance id used for path parameters.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts::plugin_instance_id(self.kind, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_kind_from_gts_id() {
        assert_eq!(
            PluginKind::from_gts_id("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
            Some(PluginKind::Auth)
        );
        assert_eq!(
            PluginKind::from_gts_id(
                "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
            ),
            Some(PluginKind::Guard)
        );
        assert_eq!(
            PluginKind::from_gts_id(
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
            ),
            Some(PluginKind::Transform)
        );
        assert_eq!(PluginKind::from_gts_id("gts.cf.core.oagw.upstream.v1~x"), None);
    }

    #[test]
    fn rate_limit_capacity_defaults_to_sustained() {
        let rl = RateLimitConfig {
            sustained: SustainedLimit {
                rate: 100,
                window: RateWindow::Minute,
            },
            burst: None,
            ..Default::default()
        };
        assert_eq!(rl.capacity(), 100.0);
        assert!((rl.refill_per_sec() - 100.0 / 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cors_wildcard_with_credentials_rejected() {
        let cors = CorsConfig {
            enabled: true,
            allow_credentials: true,
            allowed_origins: vec!["*".to_owned()],
            ..Default::default()
        };
        assert!(cors.validate().is_err());
    }

    #[test]
    fn cors_wildcard_plus_explicit_rejected() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned(), "https://a.com".to_owned()],
            ..Default::default()
        };
        assert!(cors.validate().is_err());
    }

    #[test]
    fn server_validate_rejects_mixed_ports() {
        let srv = ServerConfig {
            endpoints: vec![
                Endpoint {
                    scheme: "http".into(),
                    host: "a.example.com".into(),
                    port: 80,
                },
                Endpoint {
                    scheme: "http".into(),
                    host: "b.example.com".into(),
                    port: 8080,
                },
            ],
        };
        assert!(srv.validate(Protocol::Http).is_err());
    }

    #[test]
    fn server_validate_rejects_bad_host() {
        let srv = ServerConfig {
            endpoints: vec![Endpoint {
                scheme: "http".into(),
                host: "bad host!".into(),
                port: 80,
            }],
        };
        assert!(srv.validate(Protocol::Http).is_err());
    }
}
