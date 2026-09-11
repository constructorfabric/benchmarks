//! OAGW domain models.
//!
//! These types mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`: unknown fields are rejected, enums are
//! closed, and the documented defaults are applied by `#[serde(default)]`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::ids;

/// Endpoint scheme.
///
/// The published schema admits `https|wss|wt|grpc`; `http` is additionally
/// accepted at the field level so a lab deployment can point the gateway at a
/// local plaintext upstream. Whether a plaintext connection is actually
/// *dialled* is a separate, runtime decision governed by the gear's
/// `allow_http_upstream` setting — see [`crate::config::OagwConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext `HTTP`.
    Http,
    /// `HTTPS`.
    Https,
    /// Secure `WebSocket`.
    Wss,
    /// WebTransport.
    Wt,
    /// `gRPC` over `HTTP`/2.
    Grpc,
}

impl EndpointScheme {
    /// Default port for this scheme.
    #[must_use]
    pub fn standard_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// True when the scheme negotiates `TLS` on its own.
    #[must_use]
    pub fn is_tls(self) -> bool {
        matches!(self, Self::Https | Self::Wss)
    }

    /// The `URL` scheme used to dial this endpoint.
    #[must_use]
    pub fn url_scheme(self) -> &'static str {
        match self {
            Self::Http | Self::Wt => "http",
            Self::Https | Self::Wss | Self::Grpc => "https",
        }
    }
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Endpoint {
    /// Scheme.
    #[serde(default = "default_scheme")]
    pub scheme: EndpointScheme,
    /// Hostname or `IP` address.
    pub host: String,
    /// Port.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_scheme() -> EndpointScheme {
    EndpointScheme::Https
}

fn default_port() -> u16 {
    443
}

/// The `server` object: one or more endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ServerConfig {
    /// Endpoints.
    pub endpoints: Vec<Endpoint>,
}

/// Protocol used to reach the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum Protocol {
    /// Plain `HTTP` / `HTTP`-`JSON`.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// `gRPC`.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

/// Sharing mode for hierarchical configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
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

/// Which inbound headers are forwarded to the upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward no inbound headers (the default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything except routing and hop-by-hop headers.
    All,
}

/// Request header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if present).
    #[serde(default, skip_serializing_if = "MapCodec::is_empty")]
    // Inlined: the `MapCodec` alias is used twice with different value types,
    // which would collide on utoipa's bare `BTreeMap` component name.
    #[schema(value_type = Object, additional_properties)]
    pub set: MapCodec,
    /// Headers to add (append).
    #[serde(default, skip_serializing_if = "MapCodec::is_empty")]
    #[schema(value_type = Object, additional_properties)]
    pub add: MapCodec,
    /// Header names to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default, skip_serializing_if = "is_default_passthrough")]
    pub passthrough: Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_passthrough(p: &Passthrough) -> bool {
    matches!(p, Passthrough::None)
}

/// Response header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to set on the client response.
    #[serde(default, skip_serializing_if = "MapCodec::is_empty")]
    #[schema(value_type = Object, additional_properties)]
    pub set: MapCodec,
    /// Headers to add to the client response.
    #[serde(default, skip_serializing_if = "VecMapCodec::is_empty")]
    #[schema(value_type = Object, additional_properties)]
    pub add: VecMapCodec,
    /// Headers stripped from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Ordered map of header name → single value.
pub type MapCodec = std::collections::BTreeMap<String, String>;

/// Ordered map of header name → list of values.
pub type VecMapCodec = std::collections::BTreeMap<String, Vec<String>>;

/// Header transformation rules for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaderRules>,
    /// Rules applied to the response returned to the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// Authentication plugin binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin configuration (free-form).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: serde_json::Value,
}

/// One entry of `plugins.items`.
///
/// The published schema types the list as bare GTS identifiers, while the
/// plugin ADRs bind configuration alongside the reference; both spellings are
/// accepted and a bare string is normalised to a reference with empty config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PluginBinding {
    /// `"<gts identifier>"`.
    Ref(String),
    /// `{"plugin_ref": "...", "config": {...}, "position": 0}`.
    Bound {
        /// The plugin's GTS identifier.
        plugin_ref: String,
        /// Plugin configuration.
        #[serde(default)]
        #[schema(value_type = Object)]
        config: serde_json::Value,
        /// Position in the chain; positions must be contiguous from zero.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<u32>,
    },
}

impl PluginBinding {
    /// The referenced plugin identifier.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Ref(reference) => reference,
            Self::Bound { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The bound configuration, empty when absent.
    #[must_use]
    pub fn config(&self) -> &serde_json::Value {
        match self {
            Self::Ref(_) => &serde_json::Value::Null,
            Self::Bound { config, .. } => config,
        }
    }

    /// The declared position, when the binding carries one.
    #[must_use]
    pub fn position(&self) -> Option<u32> {
        match self {
            Self::Ref(_) => None,
            Self::Bound { position, .. } => *position,
        }
    }

    /// The reference, normalised to the `Bound` spelling.
    #[must_use]
    pub fn to_bound(&self) -> Self {
        match self {
            Self::Bound { .. } => self.clone(),
            Self::Ref(reference) => Self::Bound {
                plugin_ref: reference.clone(),
                config: serde_json::Value::Null,
                position: None,
            },
        }
    }
}

/// Plugin chain binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default, ToSchema)]
pub struct PluginsConfig {
    /// Sharing mode.
    #[serde(default, skip_serializing_if = "Sharing::is_default")]
    pub sharing: Sharing,
    /// Bound plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

impl PluginsConfig {
    fn is_default(&self) -> bool {
        self.sharing == Sharing::Private && self.items.is_empty()
    }
}

impl Sharing {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_default(&self) -> bool {
        matches!(self, Sharing::Private)
    }
}

/// Sustained rate component of a token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Sustained {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Window.
    #[serde(default = "default_window")]
    pub window: Window,
}

fn default_window() -> Window {
    Window::Second
}

/// Rate limit window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
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
    /// Duration of the window in seconds.
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

/// Burst capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Burst {
    /// Bucket capacity.
    pub capacity: u32,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    /// Token bucket, allows bursts.
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// One global counter.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per principal.
    User,
    /// One counter per client address.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Reject with `429`.
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade.
    Degrade,
}

/// Rate limit configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sharing mode.
    #[serde(default, skip_serializing_if = "Sharing::is_default")]
    pub sharing: Sharing,
    /// Algorithm.
    #[serde(default)]
    pub algorithm: Algorithm,
    /// Sustained rate.
    pub sustained: Sustained,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    /// Counter scope.
    #[serde(default)]
    pub scope: Scope,
    /// Exceeded behaviour.
    #[serde(default)]
    pub strategy: Strategy,
    /// Tokens consumed per request.
    #[serde(default)]
    pub cost: u32,
}

impl RateLimit {
    /// Bucket capacity, defaulting to the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.map_or(self.sustained.rate, |b| b.capacity)
    }
}

/// `CORS` configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode.
    #[serde(default, skip_serializing_if = "Sharing::is_default")]
    pub sharing: Sharing,
    /// Whether `CORS` is enabled. Required.
    pub enabled: bool,
    /// Allowed origins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(default = "default_cors_methods", skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

impl CorsConfig {
    /// True when `origin` is admitted, treating `*` as a wildcard.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }

    /// True when `method` is admitted cross-origin.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(method))
    }
}

/// `HTTP` method accepted by a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `PATCH`.
    Patch,
}

impl HttpMethod {
    /// The method's wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }

    /// Parses a wire method name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }
}

/// Path suffix handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject any path suffix.
    Disabled,
    /// Append the suffix to the route path.
    #[default]
    Append,
}

/// `HTTP` match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Accepted methods.
    pub methods: Vec<HttpMethod>,
    /// Path pattern.
    pub path: String,
    /// Allowed query parameters; empty allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// Path suffix handling.
    #[serde(default, skip_serializing_if = "PathSuffixMode::is_default")]
    pub path_suffix_mode: PathSuffixMode,
}

impl PathSuffixMode {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_default(&self) -> bool {
        matches!(self, PathSuffixMode::Append)
    }
}

/// `gRPC` match rules. Catalogued but never matched: no `gRPC` proxy path exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped inbound matching rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MatchRules {
    /// `HTTP` match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// `gRPC` match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// An upstream service configuration.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// System-generated identifier, `gts.cf.core.oagw.upstream.v1~{uuid}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Whether the upstream is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Categorisation tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoints.
    pub server: ServerConfig,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_default")]
    pub plugins: PluginsConfig,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// `CORS` configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

fn default_true() -> bool {
    true
}

/// A route.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// System-generated identifier, `gts.cf.core.oagw.route.v1~{uuid}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Referenced upstream.
    pub upstream_id: String,
    /// Categorisation tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub match_rules: MatchRules,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginsConfig::is_default")]
    pub plugins: PluginsConfig,
    /// Rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// Whether the route participates in matching.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// A custom plugin record.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct Plugin {
    /// Identifier, `gts.cf.core.oagw.plugin.v1~{uuid}`.
    pub id: String,
    /// Human readable name.
    pub name: String,
    /// Plugin type.
    pub plugin_type: PluginType,
    /// Starlark source of the plugin.
    pub source_code: String,
    /// Owning tenant.
    #[schema(value_type = String)]
    pub tenant_id: Uuid,
}

/// The three plugin types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    /// Credential injection.
    Auth,
    /// Validation.
    Guard,
    /// Mutation.
    Transform,
}

impl PluginType {
    /// The catalogue type id for this plugin type.
    #[must_use]
    pub fn type_id(self) -> &'static str {
        match self {
            Self::Auth => ids::AUTH_PLUGIN_TYPE_ID,
            Self::Guard => ids::GUARD_PLUGIN_TYPE_ID,
            Self::Transform => ids::TRANSFORM_PLUGIN_TYPE_ID,
        }
    }
}
