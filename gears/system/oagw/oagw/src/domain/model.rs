// @cpt-begin:cpt-cf-oagw-dod-resource-model-domain-types:p1:inst-model
//! Domain entities for the outbound API gateway.
//!
//! The shapes mirror `schemas/upstream.v1.schema.json` and
//! `schemas/route.v1.schema.json` field for field.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Configuration visibility across a tenant hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; a descendant may override it.
    Inherit,
    /// Visible; a descendant may not override it.
    Enforce,
}

/// Transport scheme of an upstream endpoint.
///
/// The supplied schema enumerates `https`, `wss`, `wt` and `grpc`. The gateway
/// additionally accepts `http`, because `allow_http_upstream` lifts the
/// HTTPS-only default posture, and tolerates `ws` as the plaintext counterpart
/// of `wss`. Accepting a value here is independent of whether a plaintext
/// connection is later made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS.
    #[default]
    Https,
    /// Plaintext WebSocket.
    Ws,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// Generic remote procedure call.
    Grpc,
}

impl Scheme {
    /// Whether this scheme carries no transport encryption.
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Self::Http | Self::Ws)
    }

    /// The port omitted from a derived alias for this scheme.
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// The scheme as it appears in an outbound URL.
    #[must_use]
    pub const fn url_scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https | Self::Wt | Self::Grpc => "https",
            Self::Ws => "ws",
            Self::Wss => "wss",
        }
    }
}

/// One reachable address of an upstream service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Transport scheme.
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port.
    #[serde(default = "default_port")]
    pub port: u16,
}

const fn default_port() -> u16 {
    443
}

/// The endpoint pool of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// One or more endpoints forming a load-balance pool.
    pub endpoints: Vec<Endpoint>,
}

/// Outbound credential configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Global type system identifier of the auth plugin.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Visibility of this block to descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// How inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassthroughMode {
    /// Forward nothing beyond what the gateway itself adds.
    #[default]
    None,
    /// Forward only the names in the allowlist.
    Allowlist,
    /// Forward every header that is not stripped.
    All,
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to overwrite.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to drop.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Names forwarded when the mode is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to overwrite.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to drop.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    #[serde(default)]
    pub request: RequestHeaderRules,
    /// Rules applied to the relayed response.
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket with a sustained refill rate and a burst capacity.
    #[default]
    TokenBucket,
    /// Sliding window over the sustained period.
    SlidingWindow,
}

/// The period a sustained rate is measured over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitWindow {
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

impl RateLimitWindow {
    /// Length of the window in seconds.
    #[must_use]
    pub const fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// What a rate limit is counted against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    /// All traffic.
    Global,
    /// Per calling tenant.
    #[default]
    Tenant,
    /// Per calling subject.
    User,
    /// Per client address.
    Ip,
    /// Per matched route.
    Route,
}

/// What happens when a rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStrategy {
    /// Answer `429` immediately.
    #[default]
    Reject,
    /// Hold the request briefly, then admit or reject it.
    Queue,
    /// Admit the request but mark it degraded.
    Degrade,
}

/// Sustained portion of a dual-rate limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Permitted requests per window.
    pub rate: u32,
    /// Length of the window.
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// Burst portion of a dual-rate limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BurstRate {
    /// Maximum instantaneous burst.
    pub capacity: u32,
}

/// Rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Visibility of this block to descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm in use.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstRate>,
    /// What the limit is counted against.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// What happens on exhaustion.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Cost of a single request.
    #[serde(default = "default_cost")]
    pub cost: u32,
}

const fn default_cost() -> u32 {
    1
}

impl RateLimitConfig {
    /// Effective burst capacity, defaulting to the sustained rate.
    #[must_use]
    pub fn burst_capacity(&self) -> u32 {
        self.burst
            .as_ref()
            .map_or(self.sustained.rate, |b| b.capacity)
    }
}

/// Cross-origin resource sharing configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Visibility of this block to descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether cross-origin checks are applied.
    pub enabled: bool,
    /// Permitted origins, or `*`.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Permitted methods.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Response headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials may be sent.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// Plugin bindings attached to an upstream or a route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Visibility of this block to descendant tenants.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered plugin references.
    #[serde(default)]
    pub items: Vec<String>,
}

/// Protocol classification of an upstream.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Protocol classification for remote procedure calls.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// A configured external service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip_deserializing)]
    pub tenant_id: Uuid,
    /// Routing key used in proxy paths.
    pub alias: String,
    /// Whether the upstream accepts traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol classification.
    pub protocol: String,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Outbound credential configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

const fn default_true() -> bool {
    true
}

/// How a path suffix is combined with the route path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject a request that carries a suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// Match rule for a plain HTTP route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Permitted methods.
    pub methods: Vec<String>,
    /// Path prefix.
    pub path: String,
    /// Permitted query parameter names.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How a trailing path suffix is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// Match rule for a remote procedure call route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Service name.
    pub service: String,
    /// Method name.
    pub method: String,
}

/// The match rule of a route; exactly one variant is present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// Plain HTTP match rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// Remote procedure call match rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A path on an upstream that inbound proxy requests are matched against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip_deserializing)]
    pub tenant_id: Uuid,
    /// The upstream this route belongs to.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match rule.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Plugin bindings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Cross-origin policy; overrides the upstream's when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// Kind of a stored custom plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    /// Injects outbound credentials.
    Auth,
    /// Validates and may reject a request.
    Guard,
    /// Mutates a request or response.
    Transform,
}

impl PluginType {
    /// The global type system base identifier for this plugin kind.
    #[must_use]
    pub const fn gts_base(self) -> &'static str {
        match self {
            Self::Auth => "gts.cf.core.oagw.auth_plugin.v1~",
            Self::Guard => "gts.cf.core.oagw.guard_plugin.v1~",
            Self::Transform => "gts.cf.core.oagw.transform_plugin.v1~",
        }
    }
}

/// A phase a transform plugin participates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "the phase names are the wire contract"
)]
pub enum TransformPhase {
    /// Before the upstream call.
    OnRequest,
    /// After a successful upstream response.
    OnResponse,
    /// After a failed upstream call.
    OnError,
}

/// A tenant-defined custom plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_field_names,
    reason = "`plugin_type` is the field name the wire contract declares"
)]
pub struct Plugin {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip_deserializing)]
    pub tenant_id: Uuid,
    /// Kind of plugin.
    pub plugin_type: PluginType,
    /// Unique name within the tenant.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Schema the plugin's configuration must satisfy.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Phases a transform plugin participates in.
    #[serde(default)]
    pub phases: Vec<TransformPhase>,
    /// Plugin source, stored verbatim and never executed in this build.
    #[serde(default)]
    pub source_code: String,
}

/// Render an anonymous global type system identifier for a resource.
#[must_use]
pub fn gts_resource_id(kind: &str, id: Uuid) -> String {
    format!("gts.cf.core.oagw.{kind}.v1~{id}")
}

/// Extract the instance part of a global type system identifier.
///
/// Returns the whole input when it carries no `~` separator.
#[must_use]
pub fn gts_instance(identifier: &str) -> &str {
    identifier
        .split_once('~')
        .map_or(identifier, |(_, rest)| rest)
}
// @cpt-end:cpt-cf-oagw-dod-resource-model-domain-types:p1:inst-model

#[cfg(test)]
mod tests {
    use super::{PluginType, Scheme, gts_instance, gts_resource_id};
    use uuid::Uuid;

    #[test]
    fn plaintext_schemes_are_identified() {
        assert!(Scheme::Http.is_plaintext());
        assert!(Scheme::Ws.is_plaintext());
        assert!(!Scheme::Https.is_plaintext());
        assert!(!Scheme::Wss.is_plaintext());
    }

    #[test]
    fn standard_ports_follow_the_scheme() {
        assert_eq!(Scheme::Http.standard_port(), 80);
        assert_eq!(Scheme::Ws.standard_port(), 80);
        assert_eq!(Scheme::Https.standard_port(), 443);
        assert_eq!(Scheme::Grpc.standard_port(), 443);
    }

    #[test]
    fn http_scheme_deserializes() {
        let scheme: Scheme = serde_json::from_str("\"http\"").expect("http is accepted");
        assert_eq!(scheme, Scheme::Http);
    }

    #[test]
    fn resource_identifier_round_trips() {
        let id = Uuid::new_v4();
        let rendered = gts_resource_id("upstream", id);
        assert!(rendered.starts_with("gts.cf.core.oagw.upstream.v1~"));
        assert_eq!(gts_instance(&rendered), id.to_string());
    }

    #[test]
    fn plugin_type_base_identifiers() {
        assert_eq!(
            PluginType::Guard.gts_base(),
            "gts.cf.core.oagw.guard_plugin.v1~"
        );
    }
}
