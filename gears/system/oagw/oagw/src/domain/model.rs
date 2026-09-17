//! Domain model mirroring `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`.
//!
//! Field names, defaults and required-ness follow the JSON schemas exactly:
//! `server`/`protocol` are required on an upstream, `upstream_id`/`match` on a
//! route, `sustained` on a `rate_limit` and `enabled` on a `cors` block, while
//! everything else falls back to the schema default. Constraints serde cannot
//! express (`minItems`, `minLength`, the `match` one-of) are enforced by
//! [`Upstream::validate`] / [`Route::validate`].
//!
//! Note: `upstream.v1` sets `additionalProperties: false`, so its types use
//! `deny_unknown_fields`; `route.v1` does not, so [`Route`] accepts (and
//! forwards) additional properties, exactly like the schema.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::domain::error::OagwError;

/// JSON-schema pattern an upstream alias must match
/// (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`).
pub const ALIAS_PATTERN: &str = "^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$";
/// JSON-schema pattern an upstream/route tag must match (`^[a-z0-9_-]+$`).
pub const TAG_PATTERN: &str = "^[a-z0-9_-]+$";
/// GTS base type of the `auth.type` identifier (`auth` plugin).
const AUTH_PLUGIN_BASE_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";

/// Default `port` of an upstream endpoint (`upstream.v1.schema.json`).
const DEFAULT_ENDPOINT_PORT: u16 = 443;
/// Default `allowed_methods` of a CORS configuration.
const DEFAULT_CORS_METHODS: [&str; 2] = ["GET", "POST"];

// ---------------------------------------------------------------------------
// Alias
// ---------------------------------------------------------------------------

/// Human-readable routing identifier of an upstream.
///
/// Constrained to [`ALIAS_PATTERN`]; invalid values are rejected on
/// deserialization and by [`Alias::try_new`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Alias(String);

impl Alias {
    /// Creates an alias, rejecting values outside [`ALIAS_PATTERN`].
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the value does not match the pattern.
    pub fn try_new(value: impl Into<String>) -> Result<Self, OagwError> {
        let value = value.into();
        if is_valid_alias(&value) {
            Ok(Self(value))
        } else {
            Err(OagwError::Validation {
                message: format!("invalid alias '{value}': must match {ALIAS_PATTERN}"),
            })
        }
    }

    /// The alias as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Alias {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Alias {
    type Error = OagwError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl AsRef<str> for Alias {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Alias {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_new(raw).map_err(|error| serde::de::Error::custom(error.to_string()))
    }
}

/// `true` when `alias` matches [`ALIAS_PATTERN`].
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    fn is_inner(b: u8) -> bool {
        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-')
    }

    match alias.as_bytes() {
        [] => false,
        [only] => only.is_ascii_lowercase() || only.is_ascii_digit(),
        [first, middle @ .., last] => {
            (first.is_ascii_lowercase() || first.is_ascii_digit())
                && (last.is_ascii_lowercase() || last.is_ascii_digit())
                && middle.iter().all(|b| is_inner(*b))
        }
    }
}

// ---------------------------------------------------------------------------
// Tag
// ---------------------------------------------------------------------------

/// Flat categorization tag (`^[a-z0-9_-]+$`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct Tag(String);

impl Tag {
    /// Creates a tag, rejecting values outside [`TAG_PATTERN`].
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the value does not match the pattern.
    pub fn try_new(value: impl Into<String>) -> Result<Self, OagwError> {
        let value = value.into();
        if is_valid_tag(&value) {
            Ok(Self(value))
        } else {
            Err(OagwError::Validation {
                message: format!("invalid tag '{value}': must match {TAG_PATTERN}"),
            })
        }
    }

    /// The tag as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Tag {
    type Error = OagwError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_new(value)
    }
}

impl<'de> Deserialize<'de> for Tag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::try_new(raw).map_err(|error| serde::de::Error::custom(error.to_string()))
    }
}

/// `true` when `tag` matches [`TAG_PATTERN`].
#[must_use]
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

// ---------------------------------------------------------------------------
// Endpoint scheme and upstream protocol
// ---------------------------------------------------------------------------

/// Scheme of a single upstream endpoint.
///
/// `http` is a legal scheme: `OagwConfig::allow_http_upstream` governs whether
/// a plaintext connection is actually dialed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP.
    Http,
    /// HTTPS (the schema default).
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC over HTTP/2.
    Grpc,
}

impl EndpointScheme {
    /// GTS fragment of the scheme, as used by the protocol identifiers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

impl fmt::Display for EndpointScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Protocol used to connect to the upstream service (GTS identifier).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// The full GTS identifier of the protocol.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            Self::Grpc => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        }
    }

    /// `true` when the upstream speaks gRPC (and is matched by `grpc` rules).
    #[must_use]
    pub const fn is_grpc(self) -> bool {
        matches!(self, Self::Grpc)
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.gts_id())
    }
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Endpoint pool of an upstream (`server` property).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamServer {
    /// At least one endpoint (`minItems: 1`).
    pub endpoints: Vec<UpstreamEndpoint>,
}

impl UpstreamServer {
    /// Validates the constraints serde cannot express.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when no endpoint is configured or a host is
    /// empty.
    pub fn validate(&self) -> Result<(), OagwError> {
        if self.endpoints.is_empty() {
            return Err(OagwError::Validation {
                message: "server.endpoints must contain at least one endpoint".to_owned(),
            });
        }
        for endpoint in &self.endpoints {
            if endpoint.host.trim().is_empty() {
                return Err(OagwError::Validation {
                    message: "server.endpoints[].host must not be empty".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// A single upstream endpoint (`scheme`, `host`, `port`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamEndpoint {
    /// Endpoint scheme; defaults to `https`.
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// Hostname or IP address of the upstream service.
    pub host: String,
    /// Endpoint port; defaults to `443`.
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
}

impl UpstreamEndpoint {
    /// `true` when the endpoint is a plaintext `http` endpoint.
    #[must_use]
    pub const fn is_plaintext(&self) -> bool {
        matches!(self.scheme, EndpointScheme::Http)
    }
}

/// Authentication plugin binding of an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Auth plugin type (`auth.type`), a GTS identifier.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<AuthType>,
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration, passed to the auth plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Authentication plugin type (`auth.type`), a GTS identifier under the
/// `gts.cf.core.oagw.auth_plugin.v1~` base type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthType(String);

impl AuthType {
    /// `noop` — no authentication.
    pub const NOOP: &'static str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// `apikey` — API key injection.
    pub const APIKEY: &'static str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// `oauth2_client_cred` — OAuth2 client credentials (form body).
    pub const OAUTH2_CLIENT_CRED: &'static str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// `oauth2_client_cred_basic` — OAuth2 client credentials (Basic header).
    pub const OAUTH2_CLIENT_CRED_BASIC: &'static str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// `basic` — catalog identifier only, no backing implementation.
    pub const BASIC: &'static str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    /// `bearer` — catalog identifier only, no backing implementation.
    pub const BEARER: &'static str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

    /// Every auth plugin type of the PRD catalogue.
    pub const BUILTIN_GTS_IDS: [&'static str; 6] = [
        Self::NOOP,
        Self::APIKEY,
        Self::OAUTH2_CLIENT_CRED,
        Self::OAUTH2_CLIENT_CRED_BASIC,
        Self::BASIC,
        Self::BEARER,
    ];

    /// Creates an auth plugin type from a GTS identifier.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the value is not a GTS identifier under
    /// `gts.cf.core.oagw.auth_plugin.v1~`.
    pub fn try_new(value: impl Into<String>) -> Result<Self, OagwError> {
        let value = value.into();
        if value.starts_with(AUTH_PLUGIN_BASE_TYPE) && value.len() > AUTH_PLUGIN_BASE_TYPE.len() {
            Ok(Self(value))
        } else {
            Err(OagwError::Validation {
                message: format!(
                    "invalid auth plugin type '{value}': must be a GTS identifier under \
                     {AUTH_PLUGIN_BASE_TYPE}"
                ),
            })
        }
    }

    /// The GTS identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `true` when the type is one of the built-in PRD catalog entries.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        Self::BUILTIN_GTS_IDS.contains(&self.0.as_str())
    }
}

impl fmt::Display for AuthType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Sharing mode of a hierarchical (tenant-inherited) configuration block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendant tenants.
    #[default]
    Private,
    /// Descendant tenants may override.
    Inherit,
    /// Descendant tenants must not override.
    Enforce,
}

impl SharingMode {
    /// `true` when descendant tenants may see (and possibly override) the value.
    #[must_use]
    pub const fn is_visible_to_descendants(self) -> bool {
        matches!(self, Self::Inherit | Self::Enforce)
    }
}

impl fmt::Display for SharingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Private => "private",
            Self::Inherit => "inherit",
            Self::Enforce => "enforce",
        })
    }
}

/// Header transformation rules for requests and responses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeaderOps {
    /// Rules applied to the request sent to the upstream.
    #[serde(default)]
    pub request: RequestHeaderOps,
    /// Rules applied to the response returned to the client.
    #[serde(default)]
    pub response: ResponseHeaderOps,
}

/// Request-side header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestHeaderOps {
    /// Headers to set (overwrite if present).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded to the upstream.
    #[serde(default)]
    pub passthrough: HeaderPassthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponseHeaderOps {
    /// Headers to set on the response to the client.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Headers to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Inbound header forwarding policy of an upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPassthrough {
    /// Forward no inbound header (the schema default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist` headers.
    Allowlist,
    /// Forward all inbound headers.
    All,
}

/// Plugin chain of an upstream or route (`plugins` property).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginBindings {
    /// Sharing mode of the plugin chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Referenced plugins; built-in plugins by GTS id, custom plugins by UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

/// A reference to a plugin: a built-in GTS identifier or a custom plugin UUID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PluginBinding(String);

impl PluginBinding {
    /// References a built-in plugin by GTS identifier.
    #[must_use]
    pub fn builtin(gts_id: impl Into<String>) -> Self {
        Self(gts_id.into())
    }

    /// References a custom plugin by UUID.
    #[must_use]
    pub fn custom(id: Uuid) -> Self {
        Self(id.to_string())
    }

    /// The binding as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The UUID, when the binding references a custom plugin.
    #[must_use]
    pub fn as_uuid(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.0).ok()
    }

    /// `true` when the binding references a built-in plugin, i.e. a GTS
    /// identifier rather than a custom plugin UUID.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.as_uuid().is_none()
    }
}

impl fmt::Display for PluginBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Rate limiting configuration (`rate_limit` property); `sustained` is
/// required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Rate limiting algorithm; defaults to `token_bucket`.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (required).
    pub sustained: RateLimitSustained,
    /// Burst capacity; defaults to `sustained.rate` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<RateLimitBurst>,
    /// Scope of the rate limit counters; defaults to `tenant`.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// Behavior when the limit is exceeded; defaults to `reject`.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request; defaults to `1`.
    #[serde(default = "default_rate_cost")]
    pub cost: u32,
}

/// Sustained rate (`sustained.rate` required, `window` defaults to `second`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitSustained {
    /// Tokens replenished per window (minimum 1).
    pub rate: u32,
    /// Replenishment window; defaults to `second`.
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// Burst configuration (`burst.capacity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitBurst {
    /// Maximum burst size; defaults to `sustained.rate` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u32>,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket, allows bursts (the schema default).
    #[default]
    TokenBucket,
    /// Sliding window, prevents boundary bursts.
    SlidingWindow,
}

/// Time window of the sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    /// One second (the schema default).
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

/// Scope of the rate limit counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One counter per process.
    Global,
    /// One counter per tenant (the schema default).
    #[default]
    Tenant,
    /// One counter per user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behavior when the rate limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject the request (the schema default).
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Serve a degraded response.
    Degrade,
}

/// CORS configuration (`cors` property); `enabled` is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled for this resource (required).
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods; defaults to `GET` and `POST`.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials are allowed; requires explicit origins.
    #[serde(default)]
    pub allow_credentials: bool,
}

/// An upstream service: the outbound target of proxied requests.
///
/// Mirrors `upstream.v1.schema.json`; `server` and `protocol` are required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// System-generated unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Whether the upstream accepts requests; defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing identifier; auto-derived from hostname endpoints when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<Alias>,
    /// Flat categorization tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Tag>,
    /// Endpoint pool (required).
    pub server: UpstreamServer,
    /// Upstream protocol (required).
    pub protocol: Protocol,
    /// Authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderOps>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginBindings>,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Upstream {
    /// Validates the schema constraints serde cannot express.
    ///
    /// # Errors
    /// [`OagwError::Validation`] describing the first violated constraint.
    pub fn validate(&self) -> Result<(), OagwError> {
        self.server.validate()
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Inbound matching rules of a route (`match` property): exactly one of
/// `http` / `grpc` must be present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteMatch {
    /// HTTP match rules, used when the upstream protocol is HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules, used when the upstream protocol is gRPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// Which protocol-specific match rule a [`RouteMatch`] selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteMatchKind {
    /// The `http` rule.
    Http,
    /// The `grpc` rule.
    Grpc,
}

impl RouteMatch {
    /// The selected match rule, or `None` when none (or both) are set.
    #[must_use]
    pub fn kind(&self) -> Option<RouteMatchKind> {
        match (self.http.is_some(), self.grpc.is_some()) {
            (true, false) => Some(RouteMatchKind::Http),
            (false, true) => Some(RouteMatchKind::Grpc),
            _ => None,
        }
    }

    /// Validates the schema constraints serde cannot express.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when no protocol rule (or both) is set, or
    /// when the selected rule violates its own constraints.
    pub fn validate(&self) -> Result<(), OagwError> {
        match (&self.http, &self.grpc) {
            (Some(http), None) => http.validate(),
            (None, Some(grpc)) => grpc.validate(),
            (Some(_), Some(_)) | (None, None) => Err(OagwError::Validation {
                message: "route.match must set exactly one of 'http' or 'grpc'".to_owned(),
            }),
        }
    }
}

/// HTTP match rules (upstream protocol is HTTP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// HTTP methods supported by the route (at least one).
    pub methods: Vec<HttpMethod>,
    /// Path pattern of the route.
    pub path: String,
    /// Allowed query parameters; empty means none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the `/path_suffix` of the proxy URL is treated; defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl HttpMatch {
    /// Validates the constraints serde cannot express.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when `methods` is empty or `path` is blank.
    pub fn validate(&self) -> Result<(), OagwError> {
        if self.methods.is_empty() {
            return Err(OagwError::Validation {
                message: "route.match.http.methods must contain at least one method".to_owned(),
            });
        }
        if self.path.trim().is_empty() {
            return Err(OagwError::Validation {
                message: "route.match.http.path must not be empty".to_owned(),
            });
        }
        Ok(())
    }
}

/// HTTP methods a route can accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HttpMethod {
    /// `GET`.
    #[serde(rename = "GET")]
    Get,
    /// `POST`.
    #[serde(rename = "POST")]
    Post,
    /// `PUT`.
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`.
    #[serde(rename = "DELETE")]
    Delete,
    /// `PATCH`.
    #[serde(rename = "PATCH")]
    Patch,
}

impl HttpMethod {
    /// The method as an HTTP token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

impl fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the `/path_suffix` of the proxy URL is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject path-suffix usage.
    Disabled,
    /// Append the suffix to the matched path (the schema default).
    #[default]
    Append,
}

impl PathSuffixMode {
    /// `true` when a `/path_suffix` may be appended to the matched path.
    #[must_use]
    pub const fn allows_suffix(self) -> bool {
        matches!(self, Self::Append)
    }
}

/// gRPC match rules (upstream protocol is gRPC).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name (e.g. `foo.v1.UserService`).
    pub service: String,
    /// RPC method name (e.g. `GetUser`).
    pub method: String,
}

impl GrpcMatch {
    /// Validates the constraints serde cannot express.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when `service` or `method` is empty.
    pub fn validate(&self) -> Result<(), OagwError> {
        if self.service.trim().is_empty() || self.method.trim().is_empty() {
            return Err(OagwError::Validation {
                message: "route.match.grpc requires a non-empty service and method".to_owned(),
            });
        }
        Ok(())
    }
}

/// A route: which upstream serves which inbound requests.
///
/// Mirrors `route.v1.schema.json`; `upstream_id` and `match` are required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// System-generated unique identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Uuid>,
    /// Flat categorization tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<Tag>,
    /// Upstream service serving this route (required).
    pub upstream_id: Uuid,
    /// Protocol-scoped inbound matching rules (required).
    #[serde(rename = "match")]
    pub match_rule: RouteMatch,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginBindings>,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration, overriding the upstream's per the sharing modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Route {
    /// Validates the schema constraints serde cannot express.
    ///
    /// # Errors
    /// [`OagwError::Validation`] describing the first violated constraint.
    pub fn validate(&self) -> Result<(), OagwError> {
        self.match_rule.validate()
    }
}

// --- schema defaults --------------------------------------------------------

fn default_endpoint_port() -> u16 {
    DEFAULT_ENDPOINT_PORT
}

fn default_true() -> bool {
    true
}

fn default_cors_methods() -> Vec<String> {
    DEFAULT_CORS_METHODS
        .iter()
        .copied()
        .map(String::from)
        .collect()
}

/// `1` — the schema default of `rate_limit.cost`.
fn default_rate_cost() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::{
        Alias, AuthType, CorsConfig, EndpointScheme, GrpcMatch, HeaderPassthrough, HttpMatch,
        PathSuffixMode, PluginBinding, Protocol, RateLimit, RateLimitSustained, Route, RouteMatch,
        SharingMode, Tag, Upstream, UpstreamEndpoint, UpstreamServer, is_valid_alias, is_valid_tag,
    };
    use crate::domain::error::{OagwError, VALIDATION_ERROR_GTS_ID};

    fn http_upstream() -> Upstream {
        Upstream {
            id: None,
            enabled: true,
            alias: None,
            tags: vec![],
            server: UpstreamServer {
                endpoints: vec![UpstreamEndpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn endpoint_scheme_round_trips_every_documented_scheme() {
        for (wire, expected) in [
            ("http", EndpointScheme::Http),
            ("https", EndpointScheme::Https),
            ("wss", EndpointScheme::Wss),
            ("wt", EndpointScheme::Wt),
            ("grpc", EndpointScheme::Grpc),
        ] {
            let parsed: EndpointScheme = serde_json::from_value(json!(wire)).expect(wire);
            assert_eq!(parsed, expected, "parse {wire}");
            assert_eq!(serde_json::to_value(expected).unwrap(), json!(wire));
            assert_eq!(parsed.to_string(), wire);
        }
        // The schema default when the key is omitted.
        let endpoint: UpstreamEndpoint = serde_json::from_value(json!({ "host": "h" })).unwrap();
        assert_eq!(endpoint.scheme, EndpointScheme::Https);
        assert_eq!(endpoint.port, 443);
    }

    #[test]
    fn protocol_round_trips_the_gts_identifiers() {
        assert_eq!(
            serde_json::to_value(Protocol::Http).unwrap(),
            json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")
        );
        let grpc: Protocol =
            serde_json::from_value(json!("gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"))
                .unwrap();
        assert_eq!(grpc, Protocol::Grpc);
        assert!(grpc.is_grpc());
        assert!(!Protocol::Http.is_grpc());
        assert_eq!(
            Protocol::Grpc.gts_id(),
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
        );
    }

    #[test]
    fn alias_pattern_accepts_and_rejects_the_documented_shapes() {
        assert!(is_valid_alias("a"));
        assert!(is_valid_alias("api"));
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("api-v1"));
        assert!(is_valid_alias("vendor.com:8443"));
        assert!(is_valid_alias("us.vendor.com"));

        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias(".leading-dot"));
        assert!(!is_valid_alias("trailing-dot."));
        assert!(!is_valid_alias("-leading-dash"));
        assert!(!is_valid_alias("trailing-dash-"));
        assert!(!is_valid_alias("UPPER.case"));
        assert!(!is_valid_alias("under_score"));
        assert!(!is_valid_alias("sp ace"));
        assert!(!is_valid_alias("api//double"));
        assert!(!is_valid_alias("port:"));

        assert!(Alias::try_new("api.openai.com").is_ok());
        let error = Alias::try_new("Bad_Alias").unwrap_err();
        assert_eq!(error.http_status(), 400);
        assert_eq!(error.gts_id(), VALIDATION_ERROR_GTS_ID);
        assert!(matches!(error, OagwError::Validation { .. }));
    }

    #[test]
    fn tag_pattern_accepts_and_rejects_the_documented_shapes() {
        assert!(is_valid_tag("llm"));
        assert!(is_valid_tag("openai_v2"));
        assert!(is_valid_tag("beta-1"));
        assert!(!is_valid_tag(""));
        assert!(!is_valid_tag("OpenAI"));
        assert!(!is_valid_tag("sp ace"));
        assert!(!is_valid_tag("dot.ted"));

        assert!(Tag::try_new("llm").is_ok());
        let error = Tag::try_new("LLM").unwrap_err();
        assert!(matches!(error, OagwError::Validation { .. }));
    }

    #[test]
    fn upstream_serializes_with_schema_defaults() {
        let upstream = http_upstream();
        let wire = serde_json::to_value(&upstream).unwrap();
        assert_eq!(
            wire,
            json!({
                "enabled": true,
                "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            })
        );

        // Round trip keeps every field.
        let parsed: Upstream = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, upstream);
    }

    #[test]
    fn upstream_rejects_unknown_properties() {
        let error = serde_json::from_value::<Upstream>(json!({
            "server": { "endpoints": [ { "host": "h" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "bogus": true,
        }))
        .unwrap_err();
        assert!(error.to_string().contains("bogus"), "got: {error}");

        let error = serde_json::from_value::<Upstream>(json!({
            "server": { "endpoints": [ { "host": "h", "extra": 1 } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        }))
        .unwrap_err();
        assert!(error.to_string().contains("extra"), "got: {error}");
    }

    #[test]
    fn upstream_rejects_malformed_alias_and_tags() {
        let error = serde_json::from_value::<Upstream>(json!({
            "alias": "Not_Valid",
            "server": { "endpoints": [ { "host": "h" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        }))
        .unwrap_err();
        assert!(error.to_string().contains("alias"), "got: {error}");

        let error = serde_json::from_value::<Upstream>(json!({
            "server": { "endpoints": [ { "host": "h" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "tags": ["Bad Tag"],
        }))
        .unwrap_err();
        assert!(error.to_string().contains("tag"), "got: {error}");
    }

    #[test]
    fn upstream_requires_server_and_protocol() {
        let missing_protocol = json!({ "server": { "endpoints": [ { "host": "h" } ] } });
        assert!(serde_json::from_value::<Upstream>(missing_protocol).is_err());

        let missing_server =
            json!({ "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" });
        assert!(serde_json::from_value::<Upstream>(missing_server).is_err());

        let mut upstream = http_upstream();
        upstream.server.endpoints.clear();
        let error = upstream.validate().unwrap_err();
        assert!(matches!(error, OagwError::Validation { .. }));
    }

    #[test]
    fn upstream_validates_endpoint_hosts() {
        let mut upstream = http_upstream();
        upstream.server.endpoints[0].host = "  ".to_owned();
        assert!(upstream.validate().is_err());
    }

    #[test]
    fn auth_type_validates_the_plugin_base_type() {
        assert!(AuthType::try_new(AuthType::APIKEY).is_ok());
        assert!(AuthType::try_new(AuthType::NOOP).is_ok());
        let error = AuthType::try_new("gts.cf.core.oagw.guard_plugin.v1~x").unwrap_err();
        assert!(matches!(error, OagwError::Validation { .. }));

        let auth_type = AuthType::try_new(AuthType::OAUTH2_CLIENT_CRED).unwrap();
        assert!(auth_type.is_builtin());
        assert_eq!(AuthType::BUILTIN_GTS_IDS.len(), 6);
        for gts_id in AuthType::BUILTIN_GTS_IDS {
            assert!(
                gts_id.starts_with("gts.cf.core.oagw.auth_plugin.v1~"),
                "{gts_id}"
            );
        }
    }

    #[test]
    fn plugin_bindings_accept_builtin_and_custom_references() {
        let custom = Uuid::new_v4();
        let wire = json!({
            "sharing": "inherit",
            "items": [
                "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
                custom,
            ],
        });
        let plugins: super::PluginBindings = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(plugins.sharing, SharingMode::Inherit);
        assert_eq!(plugins.items.len(), 2);
        assert!(plugins.items[0].as_uuid().is_none());
        assert!(plugins.items[1].as_uuid() == Some(custom));
        assert_eq!(serde_json::to_value(&plugins).unwrap(), wire);

        assert_eq!(
            PluginBinding::builtin(super::AuthType::APIKEY).to_string(),
            super::AuthType::APIKEY
        );
    }

    #[test]
    fn header_ops_round_trip_with_defaults() {
        let wire = json!({
            "request": {
                "set": { "x-trace": "abc" },
                "remove": [ "x-secret" ],
                "passthrough": "allowlist",
                "passthrough_allowlist": [ "x-user" ],
            },
            "response": { "add": { "x-gateway": "oagw" } },
        });
        let headers: super::HeaderOps = serde_json::from_value(wire).unwrap();
        assert_eq!(headers.request.passthrough, HeaderPassthrough::Allowlist);
        assert_eq!(headers.request.set["x-trace"], "abc");
        assert_eq!(headers.response.add["x-gateway"], "oagw");

        let empty: super::HeaderOps = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty.request.passthrough, HeaderPassthrough::None);
        assert!(empty.request.remove.is_empty());
    }

    #[test]
    fn rate_limit_requires_sustained_rate() {
        let error = serde_json::from_value::<RateLimit>(json!({})).unwrap_err();
        assert!(error.to_string().contains("sustained"), "got: {error}");

        let rate_limit: RateLimit = serde_json::from_value(json!({
            "sustained": { "rate": 100 },
            "burst": { "capacity": 20 },
        }))
        .unwrap();
        assert_eq!(
            rate_limit.sustained,
            RateLimitSustained {
                rate: 100,
                window: super::RateLimitWindow::Second,
            }
        );
        assert_eq!(rate_limit.algorithm, super::RateLimitAlgorithm::TokenBucket);
        assert_eq!(rate_limit.scope, super::RateLimitScope::Tenant);
        assert_eq!(rate_limit.strategy, super::RateLimitStrategy::Reject);
        assert_eq!(rate_limit.cost, 1);
        assert_eq!(rate_limit.sharing, SharingMode::Private);
        assert_eq!(rate_limit.burst.and_then(|burst| burst.capacity), Some(20));

        // `rate` is required inside `sustained`.
        let error = serde_json::from_value::<RateLimit>(json!({ "sustained": {} })).unwrap_err();
        assert!(error.to_string().contains("rate"), "got: {error}");
    }

    #[test]
    fn cors_requires_enabled() {
        let error = serde_json::from_value::<CorsConfig>(json!({})).unwrap_err();
        assert!(error.to_string().contains("enabled"), "got: {error}");

        let cors: CorsConfig = serde_json::from_value(json!({ "enabled": true })).unwrap();
        assert!(cors.enabled);
        assert_eq!(
            cors.allowed_methods,
            vec!["GET".to_owned(), "POST".to_owned()]
        );
        assert!(!cors.allow_credentials);
        assert_eq!(cors.sharing, SharingMode::Private);
    }

    fn http_route(upstream_id: Uuid) -> Route {
        Route {
            id: None,
            tags: vec![],
            upstream_id,
            match_rule: RouteMatch {
                http: Some(HttpMatch {
                    methods: vec![super::HttpMethod::Get, super::HttpMethod::Post],
                    path: "/v1/chat".to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn route_serializes_match_under_the_schema_key() {
        let upstream_id = Uuid::new_v4();
        let route = http_route(upstream_id);
        let wire = serde_json::to_value(&route).unwrap();
        assert_eq!(
            wire,
            json!({
                "upstream_id": upstream_id,
                "match": {
                    "http": {
                        "methods": ["GET", "POST"],
                        "path": "/v1/chat",
                        "path_suffix_mode": "append",
                    }
                },
            })
        );

        let parsed: Route = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, route);
        assert_eq!(parsed.match_rule.kind(), Some(super::RouteMatchKind::Http));
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn route_requires_exactly_one_match_rule() {
        let upstream_id = Uuid::new_v4();

        let missing = json!({ "upstream_id": upstream_id });
        assert!(serde_json::from_value::<Route>(missing).is_err());

        let mut both = http_route(upstream_id);
        both.match_rule.grpc = Some(GrpcMatch {
            service: "foo.v1.UserService".to_owned(),
            method: "GetUser".to_owned(),
        });
        assert!(both.match_rule.kind().is_none());
        assert!(both.validate().is_err());

        let grpc_only = Route {
            match_rule: RouteMatch {
                http: None,
                grpc: Some(GrpcMatch {
                    service: "foo.v1.UserService".to_owned(),
                    method: "GetUser".to_owned(),
                }),
            },
            ..http_route(upstream_id)
        };
        assert_eq!(
            grpc_only.match_rule.kind(),
            Some(super::RouteMatchKind::Grpc)
        );
        assert!(grpc_only.validate().is_ok());
    }

    #[test]
    fn http_match_requires_methods_and_path() {
        let upstream_id = Uuid::new_v4();
        let mut route = http_route(upstream_id);
        let RouteMatch { http, .. } = &mut route.match_rule;
        let http = http.as_mut().expect("http match");
        http.methods.clear();
        assert!(route.validate().is_err());

        let mut route = http_route(upstream_id);
        let RouteMatch { http, .. } = &mut route.match_rule;
        let http = http.as_mut().expect("http match");
        http.path = String::new();
        assert!(route.validate().is_err());
    }

    #[test]
    fn http_match_defaults_query_allowlist_and_suffix_mode() {
        let upstream_id = Uuid::new_v4();
        let route = http_route(upstream_id);
        let http = route.match_rule.http.as_ref().expect("http match");
        assert!(http.query_allowlist.is_empty());
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
        assert!(http.path_suffix_mode.allows_suffix());
        assert!(!PathSuffixMode::Disabled.allows_suffix());
    }

    #[test]
    fn grpc_match_validates_service_and_method() {
        let empty = GrpcMatch {
            service: String::new(),
            method: "GetUser".to_owned(),
        };
        assert!(empty.validate().is_err());
    }

    #[test]
    fn upstream_keeps_optional_sections_round_trip() {
        let mut upstream = http_upstream();
        upstream.alias = Some(Alias::try_new("api.openai.com").unwrap());
        upstream.tags = vec![
            Tag::try_new("llm").unwrap(),
            Tag::try_new("openai").unwrap(),
        ];
        upstream.auth = Some(super::AuthConfig {
            auth_type: Some(AuthType::try_new(super::AuthType::APIKEY).unwrap()),
            sharing: SharingMode::Enforce,
            config: Some(json!({ "header": "x-api-key" })),
        });
        upstream.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://console.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: false,
        });

        let wire = serde_json::to_value(&upstream).unwrap();
        let Value::Object(ref map) = wire else {
            panic!("expected a JSON object");
        };
        assert_eq!(map.get("alias"), Some(&json!("api.openai.com")));
        assert_eq!(map.get("tags"), Some(&json!(["llm", "openai"])));
        assert_eq!(map["auth"]["type"], json!(super::AuthType::APIKEY));
        assert_eq!(map["auth"]["sharing"], json!("enforce"));

        let parsed: Upstream = serde_json::from_value(wire).unwrap();
        assert_eq!(parsed, upstream);
        assert!(parsed.validate().is_ok());
    }
}
