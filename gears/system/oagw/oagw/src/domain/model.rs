//! Control-plane domain models (storage-normalized form).
//!
//! These mirror the JSON payload shapes in `docs/schemas/*.v1.schema.json`.
//! Sharing semantics, tag unions and rate-limit merging are applied when the
//! effective configuration for a proxy request is computed.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::gts_helpers::{PROTOCOL_GRPC_ID, PROTOCOL_HTTP_ID};

/// Current wall clock as Unix epoch milliseconds.
#[must_use]
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Sharing mode of a hierarchical configuration block.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

/// Endpoint scheme. `http` is a legal scheme; whether a plaintext connection
/// is actually made is governed by `OagwConfig::allow_http_upstream`.
#[derive(
    utoipa::ToSchema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP.
    Http,
    /// TLS HTTP.
    Https,
    /// TLS WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
}

impl EndpointScheme {
    /// Default port for the scheme (`80` for http, `443` otherwise).
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// `true` when connecting requires no TLS.
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http)
    }

    /// Lowercase wire name, as accepted in the `scheme` field.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// A single upstream endpoint.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Wire scheme.
    pub scheme: EndpointScheme,
    /// Hostname or IP literal (RFC 1123 validated on the way in).
    pub host: String,
    /// Effective port (resolved against the scheme default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Port used for the connection.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// `host[:port]` used for alias derivation.
    #[must_use]
    pub fn alias_host(&self) -> String {
        let port = self.effective_port();
        if port == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, port)
        }
    }
}

/// Serde default used by the wire DTOs.
#[must_use]
pub fn default_true() -> bool {
    true
}

/// Upstream endpoint pool. All endpoints share scheme and port.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Endpoints, in configured order (round-robin rotation order).
    pub endpoints: Vec<Endpoint>,
}

/// Upstream protocol, stored as its GTS identifier.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Protocol {
    /// HTTP/1.1 and HTTP/2.
    #[default]
    Http,
    /// gRPC (catalogued; no proxy code path).
    Grpc,
}

impl Protocol {
    /// GTS identifier string.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP_ID,
            Self::Grpc => PROTOCOL_GRPC_ID,
        }
    }

    /// Parses the GTS identifier.
    #[must_use]
    pub fn from_gts_id(value: &str) -> Option<Self> {
        if value == PROTOCOL_HTTP_ID {
            Some(Self::Http)
        } else if value == PROTOCOL_GRPC_ID {
            Some(Self::Grpc)
        } else {
            None
        }
    }
}

/// Header manipulation rules for one direction.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeaderRules {
    /// Overwrite if present.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Append, allowing duplicates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Names to drop.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Inbound forwarding policy (`none` | `allowlist` | `all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<PassthroughMode>,
    /// Names forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Inbound header forwarding policy.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward nothing but the allowlist.
    #[default]
    None,
    /// Forward only listed names.
    Allowlist,
    /// Forward everything except hop-by-hop and routing headers.
    All,
}

/// Request and response header rules for one resource.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadersConfig {
    /// Applied to the outbound request.
    #[serde(default, skip_serializing_if = "HeaderRules::is_empty")]
    pub request: HeaderRules,
    /// Applied to the inbound response.
    #[serde(default, skip_serializing_if = "HeaderRules::is_empty")]
    pub response: HeaderRules,
}

impl HeadersConfig {
    /// `true` when neither direction carries rules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.response.is_empty()
    }
}

impl HeaderRules {
    /// `true` when no rule is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.passthrough.is_none()
            && self.passthrough_allowlist.is_empty()
    }

    /// Whether the header should be forwarded under this rule set.
    #[must_use]
    pub fn forwards(&self, name: &str) -> bool {
        match self.passthrough.unwrap_or_default() {
            PassthroughMode::All => true,
            PassthroughMode::Allowlist => self
                .passthrough_allowlist
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name)),
            PassthroughMode::None => false,
        }
    }
}

/// Rate limit window unit.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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
    pub const fn secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Algorithm backing the token bucket.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[default]
    TokenBucket,
    /// Sliding window (treated as a token bucket with capacity `rate`).
    SlidingWindow,
}

/// Counter scope.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One counter for the whole deployment.
    Global,
    /// One counter per calling tenant.
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when the bucket is empty.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429 (default; `queue` and `degrade` fall back to this).
    #[default]
    Reject,
    /// Queue until a token frees up (unbounded queues are not implemented).
    Queue,
    /// Degrade the response (not implemented).
    Degrade,
}

/// Sustained throughput.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window the rate is expressed over.
    #[serde(default)]
    pub window: RateWindow,
}

impl SustainedRate {
    /// Tokens replenished per second.
    #[must_use]
    pub fn per_second(&self) -> f64 {
        f64::from(self.rate) / f64::from(u32::try_from(self.window.secs()).unwrap_or(u32::MAX))
    }
}

/// Token-bucket rate limit configuration.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Sharing mode across the hierarchy.
    pub sharing: SharingMode,
    /// Algorithm (only `token_bucket` is enforced).
    pub algorithm: RateAlgorithm,
    /// Sustained throughput.
    pub sustained: SustainedRate,
    /// Bucket capacity; defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,
    /// Counter scope.
    pub scope: RateScope,
    /// Over-limit behaviour.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u32,
    /// Emit `X-RateLimit-*` headers.
    pub response_headers: bool,
    /// Whether the block is active.
    pub enabled: bool,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::default(),
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::default(),
            },
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: 1,
            response_headers: true,
            enabled: true,
        }
    }
}

impl RateLimitConfig {
    /// Bucket capacity (`burst.capacity` or the sustained rate).
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.unwrap_or(self.sustained.rate).max(1)
    }
}

/// CORS configuration.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CorsConfig {
    /// Sharing mode across the hierarchy.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Whether CORS handling is active.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enabled: bool,
    /// Origins allowed to make cross-origin calls (`*` allows any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Methods allowed on actual cross-origin requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers allowed on actual cross-origin requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_headers: Vec<String>,
    /// Headers exposed to the browser beyond the safelist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Credentials allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_credentials: bool,
    /// Preflight cache duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<u64>,
}

impl SharingMode {
    /// `true` for [`SharingMode::Private`].
    #[must_use]
    pub const fn is_private(&self) -> bool {
        matches!(self, Self::Private)
    }

    /// `true` for [`SharingMode::Enforce`].
    #[must_use]
    pub const fn is_enforce(&self) -> bool {
        matches!(self, Self::Enforce)
    }
}

/// Plugin binding inside a chain.
#[derive(utoipa::ToSchema, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginBinding {
    /// Builtin GTS identifier, Starlark GTS identifier, or plugin UUID.
    pub id: String,
    /// Per-binding sharing override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingMode>,
    /// Plugin-local configuration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
}

/// Plugin chain configuration.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginsConfig {
    /// Sharing mode of the chain.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Ordered bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

/// Outbound authentication configuration.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub plugin_id: String,
    /// Sharing mode.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Plugin configuration keys (may hold `cred://` references).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
}

/// Path suffix handling.
#[derive(utoipa::ToSchema, Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject a supplied suffix with 400.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// HTTP matching rules.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpMatch {
    /// Allowed methods (uppercase, e.g. `GET`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    /// Path prefix pattern.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Query parameters the caller may send.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// Suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC matching rules.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service: String,
    /// RPC method name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub method: String,
}

/// Protocol-scoped match block.
#[derive(utoipa::ToSchema, Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteMatch {
    /// HTTP rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// Tenant-scoped upstream configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Normalized routing key, unique per tenant.
    pub alias: String,
    /// Flat tags.
    pub tags: BTreeSet<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Outbound auth configuration.
    pub auth: AuthConfig,
    /// Header transformation rules.
    pub headers: HeadersConfig,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate limit configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: CorsConfig,
    /// Creation instant (epoch millis).
    pub created_at: i64,
    /// Last modification instant (epoch millis).
    pub updated_at: i64,
}

impl Upstream {
    /// Host of the first endpoint (used as the outbound `Host` value).
    #[must_use]
    pub fn primary_host(&self) -> Option<&str> {
        self.server.endpoints.first().map(|e| e.host.as_str())
    }
}

/// Tenant-scoped route configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Immutable owning upstream reference.
    pub upstream_id: uuid::Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat tags.
    pub tags: BTreeSet<String>,
    /// Matching rules.
    pub route_match: RouteMatch,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate limit configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// Creation instant (epoch millis).
    pub created_at: i64,
    /// Last modification instant (epoch millis).
    pub updated_at: i64,
}

/// Stored custom (Starlark) plugin definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Human readable name.
    pub name: String,
    /// Plugin kind (auth/guard/transform).
    pub plugin_type: String,
    /// Starlark source. Never echoed into error responses.
    pub source: String,
    /// Unlinked-since instant for garbage collection (epoch millis).
    pub gc_eligible_at: Option<i64>,
    /// Creation instant (epoch millis).
    pub created_at: i64,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_defaults_and_alias_host() {
        let std_https = Endpoint {
            scheme: EndpointScheme::Https,
            host: "api.openai.com".into(),
            port: None,
        };
        assert_eq!(std_https.effective_port(), 443);
        assert_eq!(std_https.alias_host(), "api.openai.com");

        let custom = Endpoint {
            scheme: EndpointScheme::Http,
            host: "api.example".into(),
            port: Some(8080),
        };
        assert_eq!(custom.effective_port(), 8080);
        assert_eq!(custom.alias_host(), "api.example:8080");

        let std_http = Endpoint {
            scheme: EndpointScheme::Http,
            host: "api.example".into(),
            port: None,
        };
        assert_eq!(std_http.alias_host(), "api.example");
        assert!(std_http.scheme.is_plaintext());
    }

    #[test]
    fn scheme_enum_round_trip_includes_http() {
        let raw = serde_json::json!({"scheme":"http","host":"upstream","port":80});
        let ep: Endpoint = serde_json::from_value(raw).expect("http scheme must be legal");
        assert_eq!(ep.scheme, EndpointScheme::Http);
        assert_eq!(ep.effective_port(), 80);
        assert_eq!(
            serde_json::to_value(EndpointScheme::Http).unwrap(),
            serde_json::json!("http")
        );
    }

    #[test]
    fn protocol_gts_ids_round_trip() {
        assert_eq!(Protocol::Http.gts_id(), PROTOCOL_HTTP_ID);
        assert_eq!(Protocol::Grpc.gts_id(), PROTOCOL_GRPC_ID);
        assert_eq!(
            Protocol::from_gts_id(PROTOCOL_HTTP_ID),
            Some(Protocol::Http)
        );
        assert_eq!(Protocol::from_gts_id("bogus"), None);
    }

    #[test]
    fn rate_limit_capacity_falls_back_to_sustained() {
        let cfg = RateLimitConfig {
            sustained: SustainedRate {
                rate: 5,
                window: RateWindow::Second,
            },
            ..RateLimitConfig::default()
        };
        assert_eq!(cfg.capacity(), 5);
        assert!((cfg.sustained.per_second() - 5.0).abs() < f64::EPSILON);
        assert_eq!(RateWindow::Minute.secs(), 60);
        assert_eq!(RateWindow::Day.secs(), 86_400);
    }

    #[test]
    fn header_passthrough_policy() {
        let rules = HeaderRules {
            passthrough: Some(PassthroughMode::Allowlist),
            passthrough_allowlist: vec!["Accept".into()],
            ..HeaderRules::default()
        };
        assert!(rules.forwards("accept"));
        assert!(rules.forwards("ACCEPT"));
        assert!(!rules.forwards("x-secret"));
        assert!(!HeaderRules::default().forwards("x"));
        assert!(HeadersConfig::default().is_empty());
    }
}
