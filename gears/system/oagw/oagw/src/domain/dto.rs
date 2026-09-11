//! Domain DTOs mirroring `docs/schemas/*.json`.
//!
//! These are the storage and execution shapes; `api::rest::dto` holds the wire
//! shapes. The `scheme` enum is a *schema-level* enum: it admits the plaintext
//! family unconditionally, and whether a plaintext connection is actually made
//! is governed separately by `OagwConfig::allow_http_upstream`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;


/// Endpoint scheme. Admits the TLS family and the plaintext family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// TLS HTTP.
    Https,
    /// Plaintext WebSocket.
    Ws,
    /// TLS WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over HTTP/2.
    Grpc,
}

// The receiver stays `&self` on these `Copy` types so the public method
// signatures are unchanged.
#[allow(clippy::trivially_copy_pass_by_ref)]
impl Scheme {
    /// Whether the scheme is plaintext.
    #[must_use]
    pub const fn is_plaintext(&self) -> bool {
        matches!(self, Self::Http | Self::Ws)
    }

    /// Whether the scheme is a WebSocket-family scheme.
    #[must_use]
    pub const fn is_websocket(&self) -> bool {
        matches!(self, Self::Ws | Self::Wss)
    }

    /// The default port for this scheme.
    #[must_use]
    pub const fn default_port(&self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// The wire name of the scheme.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
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

/// One upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Endpoint {
    /// Endpoint scheme.
    pub scheme: Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port.
    pub port: u16,
}

impl Endpoint {
    /// Whether `host` is a literal IP address.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }

    /// The `host[:port]` form, omitting the port when it is the scheme default.
    #[must_use]
    pub fn host_with_port(&self) -> String {
        if self.port == self.scheme.default_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// `server` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ServerConfig {
    /// At least one endpoint.
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical sharing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

impl Sharing {
    /// The wire name of this sharing mode.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        }
    }
}

/// Header passthrough posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPassthrough {
    /// Forward no inbound header.
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything except the hop-by-hop and routing headers.
    All,
}

/// Outbound request header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RequestHeaderRules {
    /// Overwrite if present.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append, allowing duplicates.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Drop if present.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: HeaderPassthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Inbound response header rules.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ResponseHeaderRules {
    /// Overwrite if present.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Append, allowing duplicates.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Drop if present.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    #[serde(default)]
    pub request: Option<RequestHeaderRules>,
    /// Rules applied to the response.
    #[serde(default)]
    pub response: Option<ResponseHeaderRules>,
}

/// Rate window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
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
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
        }
    }
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Classic token bucket; permits bursts.
    #[default]
    TokenBucket,
    /// Sliding window; prevents boundary bursts.
    SlidingWindow,
}

/// Counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket for the whole gateway.
    Global,
    /// One bucket per tenant.
    #[default]
    Tenant,
    /// One bucket per authenticated subject.
    User,
    /// One bucket per client address.
    Ip,
    /// One bucket per route.
    Route,
}

/// Behaviour when the budget is exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Return `429`.
    #[default]
    Reject,
    /// Hold the request until a token is available.
    Queue,
    /// Serve a degraded response.
    Degrade,
}

/// Sustained rate component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window length.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Burst {
    /// Bucket capacity.
    pub capacity: u64,
}

/// Rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RateLimitConfig {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate; the only required field.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default)]
    pub burst: Option<Burst>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Overflow behaviour.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
}

fn default_cost() -> u64 {
    1
}

impl RateLimitConfig {
    /// Effective bucket capacity: `burst.capacity` or `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst.map_or(self.sustained.rate, |burst| burst.capacity)
    }

    /// Tokens replenished per second.
    ///
    // The widening to `f64` is the contract of this method (it returns a float
    // rate); the casts are kept explicit and exact for realistic rates.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.seconds() as f64
    }
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CorsConfig {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Whether CORS handling is active.
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl CorsConfig {
    /// Whether `origin` is allowed, exactly and case-sensitively.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        if self.allowed_origins.iter().any(|allowed| allowed == "*") {
            return true;
        }
        self.allowed_origins.iter().any(|allowed| allowed == origin)
    }

    /// Whether `method` is allowed (case-insensitive).
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// Plugin-chain configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginsConfig {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Built-in plugins by GTS identifier, custom plugins by UUID.
    #[serde(default)]
    pub items: Vec<String>,
    /// Per-plugin configuration, keyed by the plugin reference.
    #[serde(default)]
    pub config: std::collections::BTreeMap<String, serde_json::Value>,
}

impl PluginsConfig {
    /// The configuration declared for `reference`, or an empty object.
    #[must_use]
    pub fn config_for(&self, reference: &str) -> serde_json::Value {
        self.config
            .get(reference)
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::default()))
    }
}

/// Upstream protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Plaintext/TLS HTTP.
    Http,
    /// gRPC.
    Grpc,
}

impl Protocol {
    /// The GTS identifier for this protocol.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => crate::domain::gts_helpers::PROTOCOL_HTTP,
            Self::Grpc => crate::domain::gts_helpers::PROTOCOL_GRPC,
        }
    }

    /// Parses a protocol from its GTS identifier.
    #[must_use]
    pub fn from_gts_id(value: &str) -> Option<Self> {
        match value {
            crate::domain::gts_helpers::PROTOCOL_HTTP => Some(Self::Http),
            crate::domain::gts_helpers::PROTOCOL_GRPC => Some(Self::Grpc),
            _ => None,
        }
    }
}

/// An upstream service definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Routing key, unique per tenant.
    pub alias: String,
    /// Tags, unioned across the hierarchy.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp, RFC 3339.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Last-modified timestamp, RFC 3339.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// Authentication configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AuthConfig {
    /// Auth plugin identifier.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// HTTP method accepted by a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH`
    Patch,
}

impl HttpMethod {
    /// Parses an HTTP method, case-insensitively.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }

    /// The upper-case wire name.
    // The receiver stays `&self` so the public method signature is unchanged.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// How a request's path suffix is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Append the suffix to `match.http.path`.
    #[default]
    Append,
    /// Reject any request carrying a suffix.
    Disabled,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HttpMatch {
    /// Method allowlist; non-empty.
    pub methods: Vec<HttpMethod>,
    /// Path prefix.
    pub path: String,
    /// Query parameters the caller may send; empty permits none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Suffix handling.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Exactly one of HTTP or gRPC matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchRule {
    /// HTTP match.
    Http(HttpMatch),
    /// gRPC match.
    Grpc(GrpcMatch),
}

impl MatchRule {
    /// The priority of this rule: longer HTTP paths win.
    #[must_use]
    pub fn priority(&self) -> usize {
        match self {
            Self::Http(http) => http.path.len(),
            Self::Grpc(_) => 0,
        }
    }
}

/// A route definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Match rules.
    pub match_rule: MatchRule,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp, RFC 3339.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Last-modified timestamp, RFC 3339.
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// A stored plugin definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Plugin {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin family (`auth_plugin`, `guard_plugin`, `transform_plugin`).
    // `plugin_type` is the field's name in the published JSON schema, so the
    // prefix cannot be dropped.
    #[allow(clippy::struct_field_names)]
    pub plugin_type: String,
    /// Human readable name.
    pub name: String,
    /// Starlark source, served by `GET /oagw/v1/plugins/{id}/source`.
    #[serde(default)]
    pub source: String,
    /// Plugin configuration document.
    #[serde(default)]
    pub config: serde_json::Value,
    /// Earliest instant at which an unlinked plugin may be collected.
    #[serde(default)]
    pub gc_eligible_at: Option<String>,
    /// Creation timestamp, RFC 3339.
    #[serde(default)]
    pub created_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_defaults() {
        assert_eq!(Scheme::Http.default_port(), 80);
        assert_eq!(Scheme::Https.default_port(), 443);
        assert!(Scheme::Http.is_plaintext());
        assert!(Scheme::Ws.is_websocket());
    }

    #[test]
    fn endpoint_ip_detection() {
        let endpoint = Endpoint {
            scheme: Scheme::Https,
            host: "10.0.1.1".into(),
            port: 443,
        };
        assert!(endpoint.is_ip());
        assert_eq!(endpoint.host_with_port(), "10.0.1.1");
        let nonstandard = Endpoint {
            scheme: Scheme::Https,
            host: "api.openai.com".into(),
            port: 8443,
        };
        assert_eq!(nonstandard.host_with_port(), "api.openai.com:8443");
    }

    #[test]
    fn http_method_parse_is_case_insensitive() {
        assert_eq!(HttpMethod::parse("get"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::parse("PATCH"), Some(HttpMethod::Patch));
        assert_eq!(HttpMethod::parse("OPTIONS"), None);
    }
}
