//! The in-memory domain model behind the control plane.
//!
//! The shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `route.v1.schema.json`, so a model serializes straight into a
//! management-API response body. Two relaxations are deliberate:
//!
//! * `Upstream` also deserializes from a bare `endpoints` array, because the
//!   PRD's prose examples spell the endpoint pool both ways.
//! * `Route` carries an `enabled` flag (PRD §5.1) that the route schema does
//!   not list — the schema does not set `additionalProperties`, so the extra
//!   field is tolerated.

use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Endpoint scheme. `http` is a legal value here: whether a plaintext
/// connection is actually *made* is a separate, runtime question governed by
/// `allow_http_upstream` (DESIGN's `https-only` constraint is the default
/// posture that flag lifts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
}


impl Scheme {
    /// `true` when the scheme implies a TLS handshake.
    #[must_use]
    pub const fn is_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss | Self::Wt)
    }

    /// The URL scheme this maps to on the outbound hop.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https | Self::Wss => "https",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// One upstream endpoint: scheme + host + port.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Endpoint {
    /// Endpoint scheme; defaults to `https`.
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname or IP literal.
    pub host: String,
    /// Port; defaults to 443.
    #[serde(default = "default_port")]
    pub port: u16,
}

const fn default_port() -> u16 {
    443
}

/// The endpoint pool of an upstream, exactly as the JSON schema nests it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ServerConfig {
    /// Endpoint pool; at least one endpoint is required.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// `true` when no endpoint is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }
}

/// The wire protocol used on the outbound hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum Protocol {
    /// Plain HTTP (HTTP/1.1 today).
    #[default]
    Http,
    /// gRPC — accepted by the control plane, not yet proxied.
    Grpc,
}


impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // The canonical wire form is the GTS identifier; the bare word is
        // accepted on input for convenience only.
        let id = match self {
            Self::Http => crate::ids::PROTOCOL_HTTP,
            Self::Grpc => crate::ids::PROTOCOL_GRPC,
        };
        serializer.serialize_str(id)
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw == crate::ids::PROTOCOL_GRPC || raw.eq_ignore_ascii_case("grpc") {
            Ok(Self::Grpc)
        } else if raw == crate::ids::PROTOCOL_HTTP || raw.eq_ignore_ascii_case("http") {
            Ok(Self::Http)
        } else {
            Err(serde::de::Error::custom(format!(
                "{raw:?} is not a supported upstream protocol"
            )))
        }
    }
}

/// Hierarchical sharing mode for a configuration field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; a descendant may override it.
    Inherit,
    /// Visible; a descendant may not override it.
    Enforce,
}

/// Rate-limit algorithm (ADR 0003).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket — allows bursts.
    #[default]
    TokenBucket,
    /// Sliding window — prevents boundary bursts.
    SlidingWindow,
}

/// Window a sustained rate is measured over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
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
    /// Window length as a [`std::time::Duration`].
    #[must_use]
    pub const fn duration(self) -> std::time::Duration {
        match self {
            Self::Second => std::time::Duration::from_secs(1),
            Self::Minute => std::time::Duration::from_secs(60),
            Self::Hour => std::time::Duration::from_secs(3600),
            Self::Day => std::time::Duration::from_secs(86_400),
        }
    }
}

/// Counter scope for a rate limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One counter for the whole gateway instance.
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

/// What happens when a bucket is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Refuse with `429` + `Retry-After`.
    #[default]
    Reject,
    /// Hold the request until a token frees up.
    Queue,
    /// Serve a degraded response.
    Degrade,
}

/// The sustained half of a dual-rate limit (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Sustained {
    /// Tokens replenished per `window`.
    pub rate: u64,
    /// Window the rate is measured over.
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// The burst half of a dual-rate limit (ADR 0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Burst {
    /// Maximum bucket size.
    pub capacity: u64,
}

/// A rate-limit rule.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RateLimitRule {
    /// Sharing mode down the tenant hierarchy.
    #[serde(default)]
    pub sharing: Sharing,
    /// Bucket algorithm.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// The sustained rate and its window.
    pub sustained: Sustained,
    /// Bucket capacity; defaults to `sustained.rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    /// Counter scope.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// What to do when the bucket is empty.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request.
    #[serde(default = "one")]
    pub cost: u64,
}

const fn one() -> u64 {
    1
}

impl RateLimitRule {
    /// Tokens replenished per second.
    #[must_use]
    pub fn refill_per_second(&self) -> f64 {
        self.sustained.rate as f64 / self.sustained.window.duration().as_secs_f64()
    }

    /// Bucket capacity: `burst.capacity` when set, else `sustained.rate`.
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        match self.burst {
            Some(burst) => burst.capacity,
            None => self.sustained.rate,
        }
    }

    /// The stricter of two rules: the slower refill wins outright — its rate,
    /// window and capacity are kept as configured — and the larger cost wins.
    #[must_use]
    pub fn stricter_of(&self, other: &Self) -> Self {
        let slower = if self.refill_per_second() <= other.refill_per_second() {
            self
        } else {
            other
        };
        Self {
            sharing: self.sharing,
            algorithm: self.algorithm,
            sustained: slower.sustained,
            burst: slower.burst,
            scope: self.scope,
            strategy: self.strategy,
            cost: self.cost.max(other.cost),
        }
    }
}

/// CORS configuration for an upstream (ADR 0004).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CorsRule {
    /// Sharing mode down the tenant hierarchy.
    #[serde(default)]
    pub sharing: Sharing,
    /// CORS is inactive unless explicitly enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Allowed origins; `["*"]` permits any origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods; defaults to `GET` and `POST`.
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to browsers beyond the CORS-safelisted set.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Allow credentialed requests; forbidden together with `["*"]`.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_allowed_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl Default for CorsRule {
    fn default() -> Self {
        Self {
            sharing: Sharing::default(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_allowed_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

/// Which inbound request headers are forwarded upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward nothing but the routing headers OAGW itself must send.
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything (hop-by-hop headers are still stripped).
    All,
}

/// A `set` / `add` / `remove` transformation over one header map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct HeaderTransform {
    /// Headers set (overwrite when present).
    #[serde(default)]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers added (append, duplicates allowed).
    #[serde(default)]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names removed.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Inbound-header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct RequestHeaderRules {
    /// Transformations applied before forwarding.
    #[serde(flatten)]
    pub transform: HeaderTransform,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Outbound-response transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct ResponseHeaderRules {
    /// Transformations applied before returning to the client.
    #[serde(flatten)]
    pub transform: HeaderTransform,
}

/// Header transformation rules for an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct HeaderRules {
    /// Rules applied to the outbound request.
    #[serde(default)]
    pub request: RequestHeaderRules,
    /// Rules applied to the inbound response.
    #[serde(default)]
    pub response: ResponseHeaderRules,
}

impl HeaderRules {
    /// `true` when no transformation is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request.transform.set.is_empty()
            && self.request.transform.add.is_empty()
            && self.request.transform.remove.is_empty()
            && self.request.passthrough == Passthrough::None
            && self.request.passthrough_allowlist.is_empty()
            && self.response.transform.set.is_empty()
            && self.response.transform.add.is_empty()
            && self.response.transform.remove.is_empty()
    }
}

/// One plugin binding in `plugins.items[]`.
///
/// Wire shape is a bare plugin identifier string (the JSON schema's shape),
/// or — per ADR 0009's example — an object `{ "plugin_ref": ..., "config":
/// {...} }` when the binding carries inline configuration. The form used on
/// input is preserved on output so both round-trip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBinding {
    /// Plugin identifier — a built-in GTS id or a custom plugin UUID.
    pub plugin_ref: String,
    /// Inline plugin configuration, when the binding carried one.
    pub config: Option<Value>,
}

impl Serialize for PluginBinding {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.config {
            None => serializer.serialize_str(&self.plugin_ref),
            Some(config) => {
                use serde::ser::SerializeStruct;
                let mut state = serializer.serialize_struct("PluginBinding", 2)?;
                state.serialize_field("plugin_ref", &self.plugin_ref)?;
                state.serialize_field("config", config)?;
                state.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Bare(String),
            Detailed {
                plugin_ref: String,
                #[serde(default)]
                config: Option<Value>,
            },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Bare(plugin_ref) => Ok(Self {
                plugin_ref,
                config: None,
            }),
            Raw::Detailed { plugin_ref, config } => Ok(Self { plugin_ref, config }),
        }
    }
}

/// A `plugins` block: sharing mode plus the bound plugins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginSet {
    /// Sharing mode down the tenant hierarchy.
    #[serde(default)]
    pub sharing: Sharing,
    /// Bound plugins.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

impl PluginSet {
    /// `true` when the set carries no plugins.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Authentication configuration for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AuthConfig {
    /// Auth plugin identifier (`auth_plugin` GTS id).
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode down the tenant hierarchy.
    #[serde(default)]
    pub sharing: Sharing,
    /// Auth-plugin configuration.
    #[serde(default)]
    pub config: Value,
}

/// An upstream definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Upstream {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Disabled upstreams reject every proxy request with `503`.
    pub enabled: bool,
    /// Routing key used in the proxy URL.
    pub alias: String,
    /// Tags, add-only across the hierarchy.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Outbound protocol.
    pub protocol: Protocol,
    /// Credential-injection configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeaderRules::is_empty")]
    pub headers: HeaderRules,
    /// Plugins bound to this upstream.
    #[serde(default, skip_serializing_if = "PluginSet::is_empty")]
    pub plugins: PluginSet,
    /// Rate limit applied to this upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitRule>,
    /// CORS policy applied to this upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsRule>,
    /// When the resource was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// When the resource was last replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl<'de> Deserialize<'de> for Upstream {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            id: Option<Uuid>,
            #[serde(default)]
            tenant_id: Option<Uuid>,
            #[serde(default = "crate::domain::model::default_true")]
            enabled: bool,
            #[serde(default)]
            alias: Option<String>,
            #[serde(default)]
            tags: Vec<String>,
            #[serde(default)]
            server: Option<ServerConfig>,
            /// Tolerated alternative spelling of `server.endpoints`.
            #[serde(default)]
            endpoints: Option<Vec<Endpoint>>,
            protocol: Protocol,
            #[serde(default)]
            auth: Option<AuthConfig>,
            #[serde(default)]
            headers: HeaderRules,
            #[serde(default)]
            plugins: PluginSet,
            #[serde(default)]
            rate_limit: Option<RateLimitRule>,
            #[serde(default)]
            cors: Option<CorsRule>,
            #[serde(default)]
            created_at: Option<String>,
            #[serde(default)]
            updated_at: Option<String>,
        }
        let raw = Raw::deserialize(deserializer)?;
        let server = raw.server.unwrap_or(ServerConfig {
            endpoints: raw.endpoints.unwrap_or_default(),
        });
        Ok(Self {
            id: raw.id.unwrap_or_else(Uuid::new_v4),
            tenant_id: raw.tenant_id.unwrap_or_else(Uuid::nil),
            enabled: raw.enabled,
            alias: raw.alias.unwrap_or_default(),
            tags: raw.tags,
            server,
            protocol: raw.protocol,
            auth: raw.auth,
            headers: raw.headers,
            plugins: raw.plugins,
            rate_limit: raw.rate_limit,
            cors: raw.cors,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        })
    }
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HttpMatch {
    /// Methods the route serves.
    pub methods: Vec<String>,
    /// Path prefix the route serves.
    pub path: String,
    /// Query parameters the route accepts; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How `/{path_suffix}` from the proxy URL is handled.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How `/{path_suffix}` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject any request that carries a path suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// gRPC match rules.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchRule {
    /// HTTP method + path matching.
    Http(HttpMatch),
    /// gRPC service + method matching.
    Grpc(GrpcMatch),
}

/// A route definition.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Route {
    /// Server-generated identifier; ignored on input.
    #[serde(default)]
    pub id: uuid::Uuid,
    /// Owning tenant; the caller's tenant wins on input.
    #[serde(default)]
    pub tenant_id: uuid::Uuid,
    /// Disabled routes are excluded from matching.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// Tags, add-only across the hierarchy.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Upstream this route targets; immutable after creation.
    pub upstream_id: uuid::Uuid,
    /// Matching rules.
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    /// Plugins bound to this route.
    #[serde(default, skip_serializing_if = "PluginSet::is_empty")]
    pub plugins: PluginSet,
    /// Rate limit applied to this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitRule>,
    /// When the resource was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// When the resource was last replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// The plugin type a definition belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Credential injection.
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request / response mutation.
    Transform,
}

impl PluginType {
    /// GTS resource type id of this plugin type.
    #[must_use]
    pub const fn resource_type(self) -> &'static str {
        match self {
            Self::Auth => crate::ids::AUTH_PLUGIN_RESOURCE_TYPE,
            Self::Guard => crate::ids::GUARD_PLUGIN_RESOURCE_TYPE,
            Self::Transform => crate::ids::TRANSFORM_PLUGIN_RESOURCE_TYPE,
        }
    }

    /// Lowercase name used in `$filter` expressions.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// Parse the type name used in `$filter` expressions.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auth" => Some(Self::Auth),
            "guard" => Some(Self::Guard),
            "transform" => Some(Self::Transform),
            _ => None,
        }
    }
}

/// A custom plugin definition stored by the control plane.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PluginDefinition {
    /// Server-generated identifier (the UUID half of the GTS instance id).
    #[serde(default)]
    pub id: uuid::Uuid,
    /// Owning tenant; the caller's tenant wins on input.
    #[serde(default)]
    pub tenant_id: uuid::Uuid,
    /// The plugin's GTS instance id (`<type>~<uuid>`), derived on creation.
    #[serde(default)]
    pub plugin_ref: String,
    /// Plugin type.
    #[serde(rename = "type")]
    pub plugin_type: PluginType,
    /// Human-readable name.
    pub name: String,
    /// Human-readable description.
    #[serde(default)]
    pub description: String,
    /// Plugin source or configuration payload.
    #[serde(default)]
    pub config: Value,
    /// JSON schema the plugin's `config` is validated against (DESIGN §3.1's
    /// `config_schema`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Starlark source of a custom plugin, served by `GET /plugins/{id}/source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Monotonic version; plugin definitions are immutable, so always `1`.
    #[serde(default = "one_i64")]
    pub version: i64,
    /// Whether the plugin may be bound to upstreams and routes.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// When the plugin was created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

const fn one_i64() -> i64 {
    1
}

/// `true` — the serde default for the `enabled` field.
const fn default_true() -> bool {
    true
}

/// `true` when `route` serves `method`.
#[must_use]
pub fn method_matches(method: &Method, allowed: &[String]) -> bool {
    allowed
        .iter()
        .any(|candidate| candidate.trim().eq_ignore_ascii_case(method.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rule() -> RateLimitRule {
        RateLimitRule {
            sharing: Sharing::Enforce,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: Sustained {
                rate: 1,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        }
    }

    #[test]
    fn rate_limit_capacity_defaults_to_sustained_rate() {
        let rule = RateLimitRule {
            sustained: Sustained {
                rate: 10,
                window: RateLimitWindow::Second,
            },
            ..test_rule()
        };
        assert_eq!(rule.capacity(), 10);
        assert!((rule.refill_per_second() - 10.0).abs() < f64::EPSILON);

        let burst = RateLimitRule {
            burst: Some(Burst { capacity: 50 }),
            ..rule.clone()
        };
        assert_eq!(burst.capacity(), 50);
    }

    #[test]
    fn stricter_of_takes_the_slower_refill_and_larger_cost() {
        let loose = RateLimitRule {
            sustained: Sustained {
                rate: 10_000,
                window: RateLimitWindow::Second,
            },
            burst: Some(Burst { capacity: 10_000 }),
            cost: 1,
            ..test_rule()
        };
        let strict = RateLimitRule {
            sustained: Sustained {
                rate: 100,
                window: RateLimitWindow::Minute,
            },
            burst: Some(Burst { capacity: 500 }),
            cost: 3,
            ..test_rule()
        };
        let effective = loose.stricter_of(&strict);
        // 100 per minute is the slower refill.
        assert_eq!(effective.sustained.rate, 100);
        assert_eq!(effective.sustained.window, RateLimitWindow::Minute);
        assert_eq!(effective.capacity(), 500);
        assert_eq!(effective.cost, 3);
    }

    #[test]
    fn a_rate_limit_round_trips_through_the_schema_shape() {
        let value = serde_json::json!({
            "sharing": "enforce",
            "algorithm": "token_bucket",
            "sustained": { "rate": 100, "window": "second" },
            "burst": { "capacity": 500 },
            "scope": "tenant",
            "strategy": "reject",
            "cost": 1
        });
        let rule: RateLimitRule = serde_json::from_value(value.clone()).expect("parse");
        assert_eq!(rule.sustained.rate, 100);
        // `burst.capacity` is the bucket size (ADR 0003): 500, not the
        // sustained rate it is configured alongside.
        assert_eq!(rule.capacity(), 500);
        assert_eq!(serde_json::to_value(&rule).expect("serialize"), value);
    }

    #[test]
    fn scheme_defaults_to_https() {
        assert_eq!(Scheme::default(), Scheme::Https);
        assert!(!Scheme::Http.is_tls());
        assert!(Scheme::Https.is_tls());
    }

    #[test]
    fn protocol_accepts_the_gts_identifier_form() {
        let http: Protocol =
            serde_json::from_value(serde_json::json!(crate::ids::PROTOCOL_HTTP)).expect("http");
        assert_eq!(http, Protocol::Http);
        let grpc: Protocol =
            serde_json::from_value(serde_json::json!(crate::ids::PROTOCOL_GRPC)).expect("grpc");
        assert_eq!(grpc, Protocol::Grpc);
        assert_eq!(
            serde_json::to_value(Protocol::Http).expect("serialize"),
            serde_json::json!(crate::ids::PROTOCOL_HTTP)
        );
        assert!(serde_json::from_value::<Protocol>(serde_json::json!("smtp")).is_err());
    }

    #[test]
    fn plugin_binding_round_trips_both_wire_shapes() {
        let bare: PluginBinding = serde_json::from_value(serde_json::json!(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        ))
        .expect("bare string");
        assert_eq!(
            bare.plugin_ref,
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert!(bare.config.is_none());
        assert_eq!(
            serde_json::to_value(&bare).expect("serialize"),
            serde_json::json!("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
        );

        let detailed: PluginBinding = serde_json::from_value(serde_json::json!({
            "plugin_ref": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "config": { "header": "X-API-Key" }
        }))
        .expect("detailed object");
        assert_eq!(
            detailed.config,
            Some(serde_json::json!({ "header": "X-API-Key" }))
        );
        assert_eq!(
            serde_json::to_value(&detailed).expect("serialize"),
            serde_json::json!({
                "plugin_ref": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "header": "X-API-Key" }
            })
        );
    }

    #[test]
    fn match_rule_serializes_with_lowercase_discriminant() {
        let rule = MatchRule::Http(HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/v1/chat".to_owned(),
            query_allowlist: vec!["model".to_owned()],
            path_suffix_mode: PathSuffixMode::Append,
        });
        let value = serde_json::to_value(&rule).expect("serialize");
        assert!(
            value.get("http").is_some(),
            "http variant uses the `http` key"
        );
        assert!(value.get("grpc").is_none());
    }

    #[test]
    fn an_upstream_is_nested_under_server() {
        let value = serde_json::json!({
            "id": uuid::Uuid::nil(),
            "tenant_id": uuid::Uuid::nil(),
            "enabled": true,
            "alias": "api.openai.com",
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.openai.com", "port": 443 }
            ] },
            "protocol": crate::ids::PROTOCOL_HTTP
        });
        let upstream: Upstream = serde_json::from_value(value).expect("parse");
        assert_eq!(upstream.server.endpoints.len(), 1);
        assert_eq!(upstream.server.endpoints[0].host, "api.openai.com");

        // The flattened spelling from the PRD prose is tolerated too.
        let flattened = serde_json::json!({
            "endpoints": [{ "host": "api.openai.com" }],
            "protocol": crate::ids::PROTOCOL_HTTP
        });
        let upstream: Upstream = serde_json::from_value(flattened).expect("parse");
        assert_eq!(upstream.server.endpoints[0].port, 443);
        assert_eq!(upstream.server.endpoints[0].scheme, Scheme::Https);
    }

    #[test]
    fn method_matching_is_case_insensitive() {
        assert!(method_matches(
            &Method::GET,
            &["get".to_owned(), "POST".to_owned()]
        ));
        assert!(!method_matches(&Method::DELETE, &["GET".to_owned()]));
    }
}
