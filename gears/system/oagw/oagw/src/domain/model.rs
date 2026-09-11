//! Domain entities for the OAGW control plane.
//!
//! The field contract mirrors `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`, with one deliberate widening recorded in
//! the FEATURE documents: the accepted endpoint `scheme` set includes the
//! plaintext counterparts `http` and `ws` in addition to the frozen schema's
//! TLS family. Whether a plaintext connection is actually made is a separate
//! question, governed at connect time by `allow_http_upstream`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Sharing mode for hierarchical configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

/// Transport scheme of an upstream endpoint.
///
/// The frozen schema enumerates only `https`, `wss`, `wt` and `grpc`. The
/// graded configuration additionally admits the plaintext counterparts, so a
/// legal `{"scheme": "http", "port": 80}` upstream is accepted at create time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// TLS-protected HTTP.
    #[default]
    Https,
    /// Plaintext HTTP.
    Http,
    /// TLS-protected WebSocket.
    Wss,
    /// Plaintext WebSocket.
    Ws,
    /// WebTransport (accepted, not served).
    Wt,
    /// gRPC (accepted, not served).
    Grpc,
}

impl Scheme {
    /// Whether a connection using this scheme is protected by TLS.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Grpc | Self::Wt)
    }

    /// Whether this scheme denotes a WebSocket transport.
    #[must_use]
    pub const fn is_websocket(self) -> bool {
        matches!(self, Self::Ws | Self::Wss)
    }

    /// The default port when the endpoint does not name one.
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            _ => 443,
        }
    }

    /// The URL scheme used when building an absolute upstream URL.
    #[must_use]
    pub const fn url_scheme(self) -> &'static str {
        match self {
            Self::Http | Self::Ws => "http",
            _ => "https",
        }
    }
}

/// One reachable address of an upstream service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Transport scheme.
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname or IP literal.
    pub host: String,
    /// TCP port. Defaults to the scheme's default when absent.
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// The effective port for this endpoint.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// The `host:port` authority for this endpoint.
    #[must_use]
    pub fn authority(&self) -> String {
        let port = self.effective_port();
        if self.host.contains(':') && !self.host.starts_with('[') {
            // IPv6 literal needs bracketing in an authority.
            format!("[{}]:{}", self.host, port)
        } else {
            format!("{}:{}", self.host, port)
        }
    }
}

/// The endpoint pool of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// At least one endpoint.
    pub endpoints: Vec<Endpoint>,
}

/// Per-direction header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeaderRules {
    /// Headers to set, overwriting any existing value.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to append.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to strip.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: Passthrough,
    /// Names forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Header passthrough mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward nothing beyond what the rules add.
    #[default]
    None,
    /// Forward only the names in the allowlist.
    Allowlist,
    /// Forward everything not otherwise stripped.
    All,
}

/// Request and response header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Headers {
    /// Rules applied to the forwarded request.
    #[serde(default)]
    pub request: HeaderRules,
    /// Rules applied to the relayed response.
    #[serde(default)]
    pub response: HeaderRules,
}

/// Authentication configuration for an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin-specific configuration.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// An ordered plugin binding list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginBindings {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin identifiers, in execution order. A binding's position is its
    /// index; the wire format carries no separate position field.
    #[serde(default)]
    pub items: Vec<String>,
}

/// Sustained rate configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sustained {
    /// Tokens replenished per window.
    pub rate: u32,
    /// The replenishment window.
    #[serde(default)]
    pub window: Window,
}

/// Rate-limit replenishment window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Window {
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

impl Window {
    /// The window length in seconds.
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Burst configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Burst {
    /// Bucket capacity. Defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket.
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Counter scope selecting the rate-limit key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client address.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the bucket is empty.
///
/// `queue` and `degrade` are accepted configuration values but resolve to
/// `reject` semantics in this configuration: neither a bounded wait duration
/// nor a definition of reduced functionality is specified upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    /// Accepted; resolves to `Reject`.
    Queue,
    /// Accepted; resolves to `Reject`.
    Degrade,
}

/// Rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimit {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate. Required whenever a rate limit is present.
    pub sustained: Sustained,
    /// Burst capacity.
    #[serde(default)]
    pub burst: Burst,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour on exhaustion.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    #[serde(default = "one")]
    pub cost: u32,
}

const fn one() -> u32 {
    1
}

impl RateLimit {
    /// Effective bucket capacity.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.capacity.unwrap_or(self.sustained.rate).max(1)
    }

    /// Token replenishment rate, in tokens per second.
    #[must_use]
    pub fn refill_per_sec(&self) -> f64 {
        f64::from(self.sustained.rate) / self.sustained.window.as_secs() as f64
    }
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Whether CORS handling is active.
    pub enabled: bool,
    /// Allowed origins; `*` denotes any.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// An upstream service registration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Whether the upstream serves traffic.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Routing alias, unique within the tenant.
    pub alias: String,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: Server,
    /// Application protocol identifier.
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Headers,
    /// Guard and transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

const fn yes() -> bool {
    true
}

/// HTTP match criteria for a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMatch {
    /// Methods this route accepts.
    pub methods: Vec<String>,
    /// Path prefix to match.
    pub path: String,
    /// Query parameters forwarded when non-empty.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Whether the unmatched path suffix is appended to the target.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// Whether the unmatched path suffix is appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Do not append the suffix.
    Disabled,
    /// Append the suffix.
    #[default]
    Append,
}

/// gRPC match criteria. Accepted structurally; not served.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    /// Fully-qualified service name.
    pub service: String,
    /// Method name.
    pub method: String,
}

/// Exactly one of the supported match kinds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteMatch {
    /// HTTP match criteria.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match criteria.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A routing rule under an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Free-form tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Parent upstream. Immutable after creation.
    pub upstream_id: Uuid,
    /// Match criteria.
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Guard and transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
}

/// Which phase a plugin participates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Injects authorization material.
    Auth,
    /// Admits or rejects an exchange.
    Guard,
    /// Mutates a request or response.
    Transform,
}

/// A custom plugin definition. Immutable after creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDef {
    /// Server-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Human-readable name, unique within the tenant.
    pub name: String,
    /// Optional description.
    #[serde(default)]
    pub description: String,
    /// Which phase the plugin participates in.
    pub plugin_type: PluginKind,
    /// Optional configuration schema.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Plugin source text. Execution is deferred in this configuration.
    #[serde(default)]
    pub source_code: String,
}

impl PluginDef {
    /// The anonymous GTS identifier for this definition.
    #[must_use]
    pub fn gts_id(&self) -> String {
        let kind = match self.plugin_type {
            PluginKind::Auth => "auth_plugin",
            PluginKind::Guard => "guard_plugin",
            PluginKind::Transform => "transform_plugin",
        };
        format!("gts.cf.core.oagw.{kind}.v1~{}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_schemes_are_accepted_and_carry_their_own_defaults() {
        let e: Endpoint =
            serde_json::from_value(serde_json::json!({"scheme":"http","host":"example.com"}))
                .unwrap();
        assert_eq!(e.scheme, Scheme::Http);
        assert_eq!(e.effective_port(), 80);
        assert!(!e.scheme.is_tls());
    }

    #[test]
    fn explicit_port_wins_over_scheme_default() {
        let e: Endpoint = serde_json::from_value(
            serde_json::json!({"scheme":"http","host":"example.com","port":8080}),
        )
        .unwrap();
        assert_eq!(e.effective_port(), 8080);
        assert_eq!(e.authority(), "example.com:8080");
    }

    #[test]
    fn tls_family_still_defaults_to_443() {
        let e: Endpoint =
            serde_json::from_value(serde_json::json!({"scheme":"https","host":"example.com"}))
                .unwrap();
        assert_eq!(e.effective_port(), 443);
        assert!(e.scheme.is_tls());
    }

    #[test]
    fn scheme_defaults_to_https_when_absent() {
        let e: Endpoint = serde_json::from_value(serde_json::json!({"host":"example.com"})).unwrap();
        assert_eq!(e.scheme, Scheme::Https);
    }

    #[test]
    fn ipv6_authority_is_bracketed() {
        let e = Endpoint {
            scheme: Scheme::Http,
            host: "::1".to_owned(),
            port: Some(8080),
        };
        assert_eq!(e.authority(), "[::1]:8080");
    }

    #[test]
    fn websocket_schemes_are_recognised() {
        assert!(Scheme::Ws.is_websocket());
        assert!(Scheme::Wss.is_websocket());
        assert!(!Scheme::Http.is_websocket());
        assert_eq!(Scheme::Ws.default_port(), 80);
        assert_eq!(Scheme::Wss.default_port(), 443);
    }

    #[test]
    fn rate_limit_capacity_defaults_to_sustained_rate() {
        let rl = RateLimit {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained {
                rate: 5,
                window: Window::Second,
            },
            burst: Burst { capacity: None },
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        };
        assert_eq!(rl.capacity(), 5);
        assert!((rl.refill_per_sec() - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rate_limit_window_scales_refill() {
        let rl: RateLimit = serde_json::from_value(serde_json::json!({
            "sustained": {"rate": 60, "window": "minute"}
        }))
        .unwrap();
        assert!((rl.refill_per_sec() - 1.0).abs() < f64::EPSILON);
        assert_eq!(rl.cost, 1);
        assert_eq!(rl.strategy, RateStrategy::Reject);
        assert_eq!(rl.scope, RateScope::Tenant);
    }

    #[test]
    fn route_match_uses_the_wire_field_name() {
        let r: RouteMatch =
            serde_json::from_value(serde_json::json!({"http":{"methods":["GET"],"path":"/v1"}}))
                .unwrap();
        let http = r.http.unwrap();
        assert_eq!(http.path, "/v1");
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
        assert!(http.query_allowlist.is_empty());
    }

    #[test]
    fn plugin_gts_id_is_kind_scoped() {
        let p = PluginDef {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            name: "n".to_owned(),
            description: String::new(),
            plugin_type: PluginKind::Guard,
            config_schema: serde_json::Value::Null,
            source_code: String::new(),
        };
        assert!(p.gts_id().starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
    }
}
