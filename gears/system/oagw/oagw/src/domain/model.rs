//! OAGW domain model: upstreams, routes, plugins and their sub-configuration.
//!
//! Shapes follow `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`; the `sharing` mode fields drive the
//! hierarchical merge implemented in [`crate::domain::merge`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Hierarchical configuration sharing mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants must not override.
    Enforce,
}

/// Upstream endpoint transport scheme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// HTTPS (default).
    #[default]
    Https,
    /// Secure WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over TLS.
    Grpc,
    /// Cleartext HTTP. Only usable in a deployment that enables
    /// `allow_http_upstream`; the Data Plane rejects the connection otherwise
    /// (`docs/schemas/upstream.v1.schema.json` does not declare it, so it is
    /// a deployment-level extension used for local and E2E runs).
    Http,
}

impl EndpointScheme {
    /// Default port for this scheme: `80` for cleartext HTTP, `443` for every
    /// TLS-family scheme.
    #[must_use]
    pub fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// `true` when the given port is the scheme's default and therefore
    /// omitted from a derived alias.
    #[must_use]
    pub fn is_standard_port(self, port: u16) -> bool {
        port == self.standard_port()
    }
}

/// A single upstream endpoint (`host` + `port` + `scheme`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Transport scheme.
    pub scheme: EndpointScheme,
    /// Hostname or IP literal (RFC 1123 hostname validation applies).
    pub host: String,
    /// TCP port; defaults to the scheme standard (443).
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Default port applied when an endpoint omits `port`.
fn default_port() -> u16 {
    443
}

/// Upstream server pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Endpoints forming the round-robin pool (1..*).
    pub endpoints: Vec<Endpoint>,
}

/// Auth plugin binding on an upstream: the plugin reference plus its config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier (`auth.type` on the wire).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration (plugin-specific JSON keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl AuthConfig {
    /// Effective plugin reference, or `None` when unauthenticated.
    #[must_use]
    pub fn plugin_ref(&self) -> Option<&str> {
        self.auth_type.as_deref()
    }
}

/// Inbound header passthrough policy for requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound headers (default; safe).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward all inbound headers except routing and hop-by-hop headers.
    All,
}

/// Request header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if present).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded upstream.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// Allowlist used when `passthrough == allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseHeaderRules {
    /// Headers to set on the client response (overwrite if present).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add to the client response.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names stripped from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation configuration for an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadersConfig {
    /// Outbound request rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    /// Inbound response rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// Rate-limit time window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// One second (default).
    #[default]
    Second,
    /// Sixty seconds.
    Minute,
    /// 3600 seconds.
    Hour,
    /// 86400 seconds.
    Day,
}

impl RateWindow {
    /// Window length in seconds.
    #[must_use]
    pub fn as_secs(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Rate-limit refill algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket with burst allowance (default).
    #[default]
    TokenBucket,
    /// Sliding window (no boundary bursts; approximated as a strict bucket).
    SlidingWindow,
}

/// Counter scope for rate-limit buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket per process.
    Global,
    /// One bucket per tenant (default).
    #[default]
    Tenant,
    /// One bucket per authenticated subject.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per matched route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429 (default).
    #[default]
    Reject,
    /// Queue the request (implemented as rejection; queueing is future work).
    Queue,
    /// Degrade (implemented as rejection with a longer `Retry-After`).
    Degrade,
}

/// Sustained refill rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window unit (default `second`).
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity of the bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BurstCapacity {
    /// Maximum burst size (bucket capacity).
    pub capacity: u32,
}

/// Rate-limit configuration (ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Refill algorithm.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained refill rate (required).
    pub sustained: SustainedRate,
    /// Burst capacity; defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    /// Counter scope (default `tenant`).
    #[serde(default)]
    pub scope: RateScope,
    /// Behaviour on exhaustion (default `reject`).
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request (default 1).
    #[serde(default = "default_cost")]
    pub cost: u32,
    /// Emit `X-RateLimit-*` response headers (default true).
    #[serde(default = "default_true", rename = "response_headers")]
    pub response_headers: bool,
}

fn default_cost() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

impl RateLimitConfig {
    /// Bucket capacity: `burst.capacity` when configured, else `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst
            .map_or(self.sustained.rate, |burst| burst.capacity)
    }

    /// Steady-state refill rate in tokens per second.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        f64::from(self.sustained.rate)
            / f64::from(u32::try_from(self.sustained.window.as_secs().max(1)).unwrap_or(u32::MAX))
    }

    /// `Retry-After` hint when the bucket is empty, rounded up to whole seconds.
    #[must_use]
    pub fn retry_after_seconds(&self) -> u64 {
        let per_second = self.refill_per_second();
        if per_second <= 0.0 {
            return 1;
        }
        let cost = f64::from(self.cost.max(1));
        (cost / per_second).ceil().max(1.0) as u64
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorsConfig {
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled for this resource.
    pub enabled: bool,
    /// Allowed origins: `*` or absolute origins (`scheme://host[:port]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed methods (default `GET`, `POST`).
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Extra headers exposed to the browser beyond the safelist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentialed requests are allowed (requires explicit origins).
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// A plugin binding: either a bare reference string or a reference plus config.
///
/// Both shapes are accepted on the wire (`docs/schemas/upstream.v1.schema.json`
/// `plugins.items` and ADR-0002): `"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"`
/// or `{"plugin_ref": "...", "config": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginBinding {
    /// Bare reference, empty plugin config.
    Bare(String),
    /// Reference with an explicit plugin configuration object.
    With(PluginBindingWithConfig),
}

/// Reference plus config form of a [`PluginBinding`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginBindingWithConfig {
    /// Plugin reference (GTS identifier or UUID).
    pub plugin_ref: String,
    /// Plugin configuration (plugin-specific JSON keys).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

impl PluginBinding {
    /// The plugin reference.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Bare(plugin_ref) => plugin_ref,
            Self::With(with) => with.plugin_ref.as_str(),
        }
    }

    /// The plugin configuration; an empty object when not supplied.
    #[must_use]
    pub fn config(&self) -> serde_json::Value {
        match self {
            Self::Bare(_) => serde_json::Value::Object(serde_json::Map::new()),
            Self::With(with) => {
                if with.config.is_null() {
                    serde_json::Value::Object(serde_json::Map::new())
                } else {
                    with.config.clone()
                }
            }
        }
    }
}

/// Plugin chain configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Sharing mode for hierarchical merge.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Ordered plugin bindings (upstream plugins run before route plugins).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

/// HTTP request match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMatch {
    /// Allowed methods.
    pub methods: Vec<String>,
    /// Path prefix pattern (starts with `/`).
    pub path: String,
    /// Allowed query parameter names; empty allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// Whether a path suffix from the proxy URL is appended to `path`.
    #[serde(default = "default_path_suffix_mode")]
    pub path_suffix_mode: PathSuffixMode,
}

/// Path suffix handling for a matched route.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the proxy path suffix to `path` (schema default).
    #[default]
    Append,
}

/// Serde default for [`HttpMatch::path_suffix_mode`].
fn default_path_suffix_mode() -> PathSuffixMode {
    PathSuffixMode::Append
}

/// gRPC match rules (catalogued; no proxy code path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules. Exactly one of `http` / `grpc` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchConfig {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// The match kind name (`http` / `grpc`).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        if self.http.is_some() {
            "http"
        } else if self.grpc.is_some() {
            "grpc"
        } else {
            "none"
        }
    }
}

/// An upstream: tenant-scoped root configuration object for an external service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// System-generated UUID.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Routing key used in `/proxy/{alias}/...`.
    pub alias: String,
    /// Upstream protocol (GTS identifier).
    pub protocol: String,
    /// Disabled upstreams reject every request.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Server endpoint pool.
    pub server: ServerConfig,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags (add-only across the hierarchy).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation instant (unix millis). Not part of the wire schema.
    #[serde(skip)]
    pub created_at: u64,
}

/// A route: match rules plus per-route overrides for an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// System-generated UUID.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Upstream this route belongs to (immutable after creation).
    pub upstream_id: uuid::Uuid,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Sort priority for equal-prefix matches (higher wins). Schema has no
    /// `priority` property; it defaults to 0 and is echoed back.
    #[serde(default)]
    pub priority: u32,
    /// Disabled routes are skipped during resolution.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain (appended after the upstream chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Creation instant (unix millis). Not part of the wire schema.
    #[serde(skip)]
    pub created_at: u64,
}

impl Route {
    /// The `(path, priority, methods)` match key used for uniqueness checks
    /// (`DESIGN.md` §3.6 "no two enabled routes under same upstream may share
    /// `(path_prefix, priority)` for same method").
    ///
    /// The HTTP arm therefore carries the **sorted, comma-joined method
    /// allowlist**: `GET /v1/x` and `POST /v1/x` are distinct keys and may
    /// coexist on the same upstream, while two routes with overlapping
    /// method sets on the same `(path, priority)` conflict. Method comparison
    /// is case-insensitive at match time, so the key normalizes to upper case.
    #[must_use]
    pub fn match_key(&self) -> String {
        match &self.match_config.http {
            Some(http) => format!(
                "http|{}|{}|{}",
                http.path,
                self.priority,
                Self::method_key(&http.methods)
            ),
            None => match &self.match_config.grpc {
                Some(grpc) => format!("grpc|{}|{}|{}", grpc.service, grpc.method, self.priority),
                None => format!("none|{0}", self.priority),
            },
        }
    }

    /// Sorted, comma-joined, upper-cased method allowlist of an HTTP match.
    fn method_key(methods: &[String]) -> String {
        let mut normalized: Vec<String> = methods
            .iter()
            .map(|method| method.to_ascii_uppercase())
            .collect();
        normalized.sort_unstable();
        normalized.dedup();
        normalized.join(",")
    }
}

/// Plugin type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Credential injection; one per upstream.
    Auth,
    /// Validation / policy; many per upstream or route.
    Guard,
    /// Request / response mutation; many per upstream or route.
    Transform,
}

impl PluginType {
    /// The GTS base type id for this plugin type.
    #[must_use]
    pub fn base_type_id(self) -> &'static str {
        match self {
            Self::Auth => crate::domain::gts::AUTH_PLUGIN_TYPE_ID,
            Self::Guard => crate::domain::gts::GUARD_PLUGIN_TYPE_ID,
            Self::Transform => crate::domain::gts::TRANSFORM_PLUGIN_TYPE_ID,
        }
    }

    /// Parses the `oagw_plugin.plugin_type` column value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }
}

/// A tenant-defined (Starlark) plugin. Immutable after creation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    /// System-generated UUID (the GTS instance tail).
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Plugin type (`auth` | `guard` | `transform`).
    pub plugin_type: PluginType,
    /// Unique name within the tenant.
    pub name: String,
    /// JSON Schema describing the accepted plugin configuration.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Starlark source code.
    #[serde(default)]
    pub source_code: String,
    /// Last request that resolved this plugin (unix millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    /// When the plugin became eligible for garbage collection (unix millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<u64>,
    /// Creation instant (unix millis).
    #[serde(skip)]
    pub created_at: u64,
}

/// Unix-milliseconds timestamp for "now".
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn parses_upstream_wire_shape() {
        let json = r#"{
            "id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "tenant_id": "6ba7b811-9dad-11d1-80b4-00c04fd430c8",
            "alias": "api.openai.com",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "enabled": true,
            "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "sharing": "inherit",
                "config": { "secret_ref": "cred://openai" }
            },
            "headers": {
                "request": { "set": { "x-a": "b" }, "remove": [ "x-c" ], "passthrough": "allowlist", "passthrough_allowlist": [ "accept" ] },
                "response": { "remove": [ "server" ] }
            },
            "rate_limit": { "sharing": "enforce", "sustained": { "rate": 100, "window": "minute" }, "burst": { "capacity": 500 }, "scope": "tenant", "strategy": "reject", "cost": 2 },
            "cors": { "enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET","POST"], "expose_headers": ["x-request-id"], "allow_credentials": false },
            "plugins": { "sharing": "private", "items": [ "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1", { "plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1", "config": { "force": true } } ] },
            "tags": ["llm"]
        }"#;
        let upstream: Upstream = serde_json::from_str(json).unwrap();
        assert_eq!(upstream.protocol, crate::domain::gts::PROTOCOL_HTTP);
        assert_eq!(upstream.server.endpoints.len(), 1);
        assert_eq!(upstream.server.endpoints[0].port, 443);
        let auth = upstream.auth.unwrap();
        assert_eq!(auth.sharing, SharingMode::Inherit);
        let headers = upstream.headers.unwrap();
        assert_eq!(
            headers.request.unwrap().passthrough,
            PassthroughMode::Allowlist
        );
        let rate = upstream.rate_limit.unwrap();
        assert_eq!(rate.capacity(), 500);
        assert_eq!(rate.refill_per_second(), 100.0 / 60.0);
        assert!(upstream.cors.unwrap().enabled);
        let plugins = upstream.plugins.unwrap();
        assert_eq!(plugins.items.len(), 2);
        assert_eq!(
            plugins.items[0].plugin_ref(),
            crate::domain::gts::GUARD_PLUGIN_REQUIRED_HEADERS
        );
        assert_eq!(
            plugins.items[1].config(),
            serde_json::json!({ "force": true })
        );
    }

    #[test]
    fn plugin_binding_accepts_bare_string_and_object() {
        let bare: PluginBinding = serde_json::from_str("\"abc\"").unwrap();
        assert_eq!(bare.plugin_ref(), "abc");
        assert!(bare.config().is_object());

        let with: PluginBinding =
            serde_json::from_str(r#"{"plugin_ref": "p", "config": {"k": 1}}"#).unwrap();
        assert_eq!(with.plugin_ref(), "p");
        assert_eq!(with.config(), serde_json::json!({"k": 1}));

        let no_config: PluginBinding = serde_json::from_str(r#"{"plugin_ref": "p"}"#).unwrap();
        assert_eq!(no_config.plugin_ref(), "p");
        assert!(no_config.config().is_object());
    }

    #[test]
    fn route_match_defaults() {
        let json = r#"{
            "id": "0b9e6b1e-1f2a-4c3d-8e4f-5a6b7c8d9e0f",
            "tenant_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "upstream_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        }"#;
        let route: Route = serde_json::from_str(json).unwrap();
        let http = route.match_config.http.clone().unwrap();
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
        assert!(http.query_allowlist.is_empty());
        assert!(route.enabled);
        assert_eq!(route.priority, 0);
        assert_eq!(route.match_key(), "http|/v1|0|GET");
    }

    fn http_route(methods: &[&str], path: &str, priority: u32) -> Route {
        Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            upstream_id: uuid::Uuid::new_v4(),
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            priority,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: Vec::new(),
            created_at: 0,
        }
    }

    #[test]
    fn method_split_routes_have_distinct_match_keys() {
        let get = http_route(&["GET"], "/v1/x", 0);
        let post = http_route(&["POST"], "/v1/x", 0);
        assert_ne!(
            get.match_key(),
            post.match_key(),
            "F-010: methods are part of the key"
        );
        assert_eq!(get.match_key(), "http|/v1/x|0|GET");
        assert_eq!(post.match_key(), "http|/v1/x|0|POST");
    }

    #[test]
    fn match_key_methods_are_normalized_and_ordered() {
        let route = http_route(&["post", "GET", "get"], "/v1", 3);
        assert_eq!(route.match_key(), "http|/v1|3|GET,POST");
        // A wider allowlist never collapses onto a single-method key.
        assert_ne!(
            route.match_key(),
            http_route(&["GET"], "/v1", 3).match_key()
        );
    }

    #[test]
    fn sharing_modes_parse() {
        assert_eq!(
            serde_json::from_str::<SharingMode>("\"enforce\"").unwrap(),
            SharingMode::Enforce
        );
        let upstream: Upstream = serde_json::from_str(
            r#"{
                "id": "0b9e6b1e-1f2a-4c3d-8e4f-5a6b7c8d9e0f",
                "tenant_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
                "alias": "a.example.com",
                "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
            }"#,
        )
        .unwrap();
        assert!(upstream.enabled, "enabled defaults to true");
        assert!(upstream.auth.is_none());
        assert!(upstream.tags.is_empty());
    }
}
