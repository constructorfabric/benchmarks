//! Domain configuration models — the wire schemas for upstreams, routes and
//! plugins (mirrors `schemas/upstream.v1.schema.json` and
//! `schemas/route.v1.schema.json`).
//!
//! Deserialization is tolerant (unknown fields ignored) so forward-compatible
//! configurations keep working; validation is explicit in the service layer.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Sharing mode for hierarchical configuration (DESIGN §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants can override.
    Inherit,
    /// Descendants cannot override; the ancestor limit always applies.
    Enforce,
}

/// A single upstream server endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// `https` (default), `http`, `wss`, `wt`, `grpc`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Hostname or IP literal.
    pub host: String,
    /// Port; absent means the scheme default (http→80, else 443).
    #[serde(default)]
    pub port: Option<u16>,
}

fn default_scheme() -> String {
    "https".to_owned()
}

impl Endpoint {
    /// Effective port with scheme-based defaulting.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| {
            if self.scheme.eq_ignore_ascii_case("http") {
                80
            } else {
                443
            }
        })
    }

    /// True when the endpoint uses the well-known port for its scheme
    /// (omitted from derived aliases — DESIGN §3.2 "Alias Resolution").
    #[must_use]
    pub fn uses_standard_port(&self) -> bool {
        self.effective_port()
            == if self.scheme.eq_ignore_ascii_case("http") {
                80
            } else {
                443
            }
    }
}

/// Server configuration: one or more endpoints (load-balanced pool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin binding for an upstream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    /// GTS identifier of the auth plugin type (e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    #[serde(default, rename = "type")]
    pub plugin_type: Option<String>,
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    /// Opaque plugin configuration (interpreted by the plugin).
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Request/response header transformation rules (DESIGN §3.2 "Headers
/// Transformation").
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: RequestHeadersConfig,
    #[serde(default)]
    pub response: ResponseHeadersConfig,
}

/// Inbound request header rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestHeadersConfig {
    /// Headers to set (overwrite).
    #[serde(default)]
    pub set: HashMap<String, String>,
    /// Headers to add (append).
    #[serde(default)]
    pub add: HashMap<String, String>,
    /// Header names to strip from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Passthrough policy: `none` (default), `allowlist`, `all`.
    #[serde(default)]
    pub passthrough: Option<String>,
    /// Headers forwarded when `passthrough == "allowlist"`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Outbound response header rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseHeadersConfig {
    #[serde(default)]
    pub set: HashMap<String, String>,
    #[serde(default)]
    pub add: HashMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// A plugin binding inside a plugin chain.
///
/// Accepts both the plain GTS identifier form (`items: ["gts..."]`) and the
/// object form with per-binding config (`items: [{"plugin_ref": "...",
/// "config": {...}}]` — see ADR-0009's example).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBinding {
    /// Plain GTS identifier (or custom plugin UUID).
    Id(String),
    /// Object form with optional per-binding configuration.
    WithConfig {
        #[serde(rename = "plugin_ref")]
        plugin_ref: String,
        #[serde(default)]
        config: serde_json::Value,
        #[serde(default)]
        position: Option<usize>,
    },
}

impl PluginBinding {
    /// The referenced plugin GTS identifier.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            PluginBinding::Id(id) => id,
            PluginBinding::WithConfig { plugin_ref, .. } => plugin_ref,
        }
    }

    /// Per-binding plugin config (empty JSON object when unspecified).
    #[must_use]
    pub fn config(&self) -> serde_json::Value {
        match self {
            PluginBinding::Id(_) => serde_json::Value::Object(Default::default()),
            PluginBinding::WithConfig { config, .. } => config.clone(),
        }
    }
}

/// Plugin chain (auth is scalar; guard/transform chains use this).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginChainConfig {
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Rate limiting configuration (ADR-0003 dual-rate token bucket).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    /// `token_bucket` (default) or `sliding_window`.
    #[serde(default)]
    pub algorithm: Option<String>,
    /// Sustained rate: tokens replenished per window.
    #[serde(default)]
    pub sustained: Option<SustainedRate>,
    /// Burst allowance.
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    /// Counter scope: `global`, `tenant`, `user`, `ip`, `route`.
    #[serde(default)]
    pub scope: Option<String>,
    /// `reject` (default), `queue`, `degrade`.
    #[serde(default)]
    pub strategy: Option<String>,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Include `X-RateLimit-*` / `Retry-After` headers.
    #[serde(default = "default_true")]
    pub response_headers: bool,
}

const fn default_cost() -> u32 {
    1
}
const fn default_true() -> bool {
    true
}

/// Sustained rate definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SustainedRate {
    pub rate: u32,
    /// `second` (default), `minute`, `hour`, `day`.
    #[serde(default = "default_window")]
    pub window: String,
}

fn default_window() -> String {
    "second".to_owned()
}

/// Burst configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BurstConfig {
    pub capacity: u32,
}

/// CORS configuration (ADR-0004, per-upstream/route).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Validate the `allow_credentials` + wildcard prohibition (ADR-0004).
    #[must_use]
    pub fn validation_error(&self) -> Option<String> {
        if self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*") {
            Some(
                "invalid cors config: cannot use 'allow_credentials' with wildcard origin '*'"
                    .to_owned(),
            )
        } else {
            None
        }
    }

    /// True when CORS is enabled and the request has an `Origin` that must
    /// be validated (only cross-origin requests are subject to CORS).
    #[must_use]
    pub fn is_origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    #[must_use]
    pub fn is_method_allowed(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Protocol GTS identifiers (DESIGN §3.1).
pub mod protocols {
    pub const HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    pub const GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";
}

/// Upstream configuration (wire schema `upstream.v1.schema.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamConfig {
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
    /// Routing alias. Omitting it triggers auto-derivation for hostname
    /// endpoints; required for IP-based / non-derivable endpoints.
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: String,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub headers: HeadersConfig,
    #[serde(default)]
    pub plugins: PluginChainConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

const fn default_true_bool() -> bool {
    true
}

/// Match rules for a route. The wire schema (`route.v1.schema.json`)
/// requires exactly one of `http`/`grpc` inside `match`; the struct keeps
/// both optional so a missing `match` block deserializes tolerantly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MatchConfig {
    /// HTTP match keys (used when the upstream protocol is HTTP).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match keys (the route registrar accepts the shape for catalog
    /// completeness; the proxy data plane is HTTP-only for now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// HTTP match keys (used when the upstream protocol is HTTP).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpMatch {
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path pattern; longest-prefix match with priority tiebreak.
    pub path: String,
    /// Allowlisted query parameters (empty ⇒ none forwarded).
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// `append` (default) or `disabled` — how `/path_suffix` is treated.
    #[serde(default = "default_suffix_mode")]
    pub path_suffix_mode: String,
}

fn default_suffix_mode() -> String {
    "append".to_owned()
}

/// gRPC match keys (route registrar accepts the shape for catalog
/// completeness; the proxy data plane is HTTP-only for now).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Tolerant default match: HTTP match on `/` (all methods). Used only
/// when a route body omits `match` entirely.
#[must_use]
fn default_route_match() -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: Vec::new(),
            path: "/".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: "append".to_owned(),
        }),
        grpc: None,
    }
}

/// Route configuration (wire schema `route.v1.schema.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// Reference to the owning upstream (UUID or anonymous GTS id).
    pub upstream_id: String,
    /// Match rules (`match` on the wire — `schemas/route.v1.schema.json`).
    #[serde(default = "default_route_match", rename = "match")]
    pub match_config: MatchConfig,
    #[serde(default)]
    pub plugins: PluginChainConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
}

impl RouteConfig {
    /// The HTTP match if this route is HTTP-scoped.
    #[must_use]
    pub fn http_match(&self) -> Option<&HttpMatch> {
        self.match_config.http.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Stored (tenant-scoped, ID'd) entities
// ---------------------------------------------------------------------------

/// A stored upstream with its canonical (enforced) alias.
#[derive(Debug, Clone)]
pub struct StoredUpstream {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// The enforced routing alias (derived or explicit, normalized).
    pub alias: String,
    /// Whether the alias was auto-derived (`true`) or user-provided.
    pub alias_derived: bool,
    pub config: UpstreamConfig,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A stored route.
#[derive(Debug, Clone)]
pub struct StoredRoute {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub upstream_id: Uuid,
    pub config: RouteConfig,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Plugin type discriminant (DESIGN §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginKind {
    Auth,
    Guard,
    Transform,
}

impl PluginKind {
    /// Parse a plugin GTS identifier into its kind plus instance part.
    pub fn classify(gts_id: &str) -> Option<Self> {
        let lower = gts_id.to_ascii_lowercase();
        if lower.starts_with("gts.cf.core.oagw.auth_plugin.v1~") {
            Some(Self::Auth)
        } else if lower.starts_with("gts.cf.core.oagw.guard_plugin.v1~") {
            Some(Self::Guard)
        } else if lower.starts_with("gts.cf.core.oagw.transform_plugin.v1~") {
            Some(Self::Transform)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The anonymous GTS type prefix for UUID-backed plugins of this kind.
    #[must_use]
    pub const fn gts_type_prefix(&self) -> &'static str {
        match self {
            Self::Auth => "gts.cf.core.oagw.auth_plugin.v1",
            Self::Guard => "gts.cf.core.oagw.guard_plugin.v1",
            Self::Transform => "gts.cf.core.oagw.transform_plugin.v1",
        }
    }
}

/// A stored custom (UUID-backed) plugin.
#[derive(Debug, Clone)]
pub struct StoredPlugin {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub kind: PluginKind,
    /// The full anonymous GTS identifier
    /// `gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`.
    pub gts_id: String,
    pub source_code: String,
    pub config_schema: serde_json::Value,
    pub created_at: u64,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_scheme_default_ports() {
        let http = Endpoint {
            scheme: "http".into(),
            host: "127.0.0.1".into(),
            port: None,
        };
        assert_eq!(http.effective_port(), 80);
        let https = Endpoint {
            scheme: "https".into(),
            host: "example.com".into(),
            port: None,
        };
        assert_eq!(https.effective_port(), 443);
        assert!(https.uses_standard_port());
        let custom = Endpoint {
            scheme: "https".into(),
            host: "example.com".into(),
            port: Some(8443),
        };
        assert!(!custom.uses_standard_port());
    }

    #[test]
    fn plugin_binding_accepts_both_forms() {
        let plain: PluginBinding = serde_json::from_str(
            r#""gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1""#,
        )
        .unwrap();
        assert_eq!(
            plain.plugin_ref(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );

        let with_config: PluginBinding = serde_json::from_str(
            r#"{"plugin_ref":"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1","config":{"required_request_headers":"x-correlation-id"}}"#,
        )
        .unwrap();
        assert_eq!(
            with_config.plugin_ref(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert_eq!(
            with_config.config()["required_request_headers"],
            "x-correlation-id"
        );
    }

    #[test]
    fn cors_validation_rejects_wildcard_with_credentials() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..Default::default()
        };
        assert!(cors.validation_error().is_some());
        let ok = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allow_credentials: true,
            ..Default::default()
        };
        assert!(ok.validation_error().is_none());
    }

    #[test]
    fn route_parses_http_match() {
        let json = serde_json::json!({
            "upstream_id": "gts.cf.core.oagw.upstream.v1~12345678-1234-1234-1234-123456789012",
            "match": { "http": { "methods": ["GET", "POST"], "path": "/v1" } }
        });
        let route: RouteConfig = serde_json::from_value(json).unwrap();
        let m = route.http_match().unwrap();
        assert_eq!(m.path, "/v1");
        assert_eq!(m.methods, vec!["GET", "POST"]);
        assert_eq!(m.path_suffix_mode, "append");
    }
}
