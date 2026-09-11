//! The `Upstream` entity and its value objects.
//!
//! An upstream is a tenant-scoped definition of an external service: where it lives
//! (endpoint pool), how to authenticate to it, how to transform headers, how to
//! rate-limit calls, which CORS origins may reach it and which plugins apply.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::error::{ErrorKind, OagwError};

/// Identifier prefix of the upstream GTS type.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1";

/// Endpoint schemes the upstream field accepts. Whether a plaintext connection is
/// actually made for `http`/`ws` is governed separately by the gear's
/// `allow_http_upstream` setting.
pub const SCHEMES: [&str; 5] = ["http", "https", "ws", "wss", "wt"];

/// Protocols an upstream may speak.
pub const PROTOCOLS: [&str; 2] = [
    "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
];

/// Builds a full GTS identifier for an upstream.
#[must_use]
pub fn upstream_id(id: Uuid) -> String {
    format!("{UPSTREAM_TYPE}~{id}")
}

/// A single outbound target: scheme, host and port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Endpoint scheme (`http`, `https`, `ws`, `wss` or `wt`).
    pub scheme: String,
    /// RFC 1123 hostname or IP literal.
    pub host: String,
    /// Port, 1–65535.
    pub port: u16,
}

impl Endpoint {
    /// The host normalized to ASCII lowercase with any trailing dot stripped.
    #[must_use]
    pub fn normalized_host(&self) -> String {
        alias::normalize(&self.host)
    }
}

/// The set of endpoints an upstream may be reached on. Every endpoint in a pool shares
/// the same protocol, scheme and port.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// At least one endpoint.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// How the gateway authenticates to the upstream. Carries credential *references*,
/// never credential material.
#[derive(Clone, PartialEq, Eq, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Built-in auth plugin identifier.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Sharing mode for the resolved credential.
    #[serde(default = "default_sharing")]
    pub sharing: String,
    /// Plugin configuration, including `cred://` references. Redacted on output.
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

fn default_sharing() -> String {
    "tenant".to_owned()
}

fn default_cost() -> u64 {
    1
}

/// Redacted rendering of the auth configuration: only the plugin type and sharing mode
/// are shown, never the referenced credential values.
impl std::fmt::Debug for AuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthConfig")
            .field("type", &self.auth_type)
            .field("sharing", &self.sharing)
            .field("config", &"<redacted>")
            .finish()
    }
}

impl Serialize for AuthConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AuthConfig", 3)?;
        s.serialize_field("type", &self.auth_type)?;
        s.serialize_field("sharing", &self.sharing)?;
        s.serialize_field("config", &"<redacted>")?;
        s.end()
    }
}

/// Header transformation rules applied to the request before it is forwarded and to the
/// response before it is returned to the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(default)]
    pub request: RequestHeaders,
    /// Response-side rules.
    #[serde(default)]
    pub response: ResponseHeaders,
}

/// Request-side header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// Headers set unconditionally, overwriting anything the caller sent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<HeaderOp>,
    /// Headers added when the caller did not already send them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<HeaderOp>,
    /// Headers removed from the forwarded request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Whether the caller's headers pass through at all.
    #[serde(default)]
    pub passthrough: PassthroughMode,
    /// When `passthrough` is `allowlist`, the only caller headers that survive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response-side header rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// Headers set unconditionally on the relayed response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub set: Vec<HeaderOp>,
    /// Headers added when the upstream did not already send them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub add: Vec<HeaderOp>,
    /// Headers removed from the relayed response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// A single header assignment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeaderOp {
    /// Header name.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// How much of the caller's original header set is forwarded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Nothing passes through; the upstream sees only the configured headers.
    None,
    /// Only the allowlisted caller headers pass through.
    Allowlist,
    /// Everything passes through (minus hop-by-hop and routing headers).
    #[default]
    All,
}

/// Rate-limit policy attached to an upstream or a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sharing mode for the counter.
    #[serde(default = "default_sharing")]
    pub sharing: String,
    /// Limiting algorithm.
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate over a window. Required.
    pub sustained: SustainedRate,
    /// Optional burst capacity, defaulting to the sustained rate.
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    /// What the counter is keyed on.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// What happens when the limit is exceeded.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed by one request.
    #[serde(default = "default_cost")]
    pub cost: u64,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            sharing: default_sharing(),
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 1,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: default_cost(),
        }
    }
}

/// Limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Refills continuously at the sustained rate.
    TokenBucket,
    /// Counts requests inside a rolling window.
    SlidingWindow,
}

/// Sustained rate and its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Requests per window.
    pub rate: u64,
    /// Window length.
    pub window: RateWindow,
}

/// Window unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    Second,
    Minute,
    Hour,
    Day,
}

impl RateWindow {
    /// The window as a `Duration`.
    #[must_use]
    // `Duration::from_mins` and friends are not stable as const constructors.
    #[allow(clippy::duration_suboptimal_units)]
    pub const fn duration(self) -> std::time::Duration {
        match self {
            Self::Second => std::time::Duration::from_secs(1),
            Self::Minute => std::time::Duration::from_secs(60),
            Self::Hour => std::time::Duration::from_secs(3_600),
            Self::Day => std::time::Duration::from_secs(86_400),
        }
    }
}

/// Burst configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BurstConfig {
    /// Bucket capacity above the sustained rate.
    pub capacity: u64,
}

/// Counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One counter for the whole gear.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Strategy applied when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Answer 429 with retry guidance.
    #[default]
    Reject,
    /// Queue the request (implemented as an immediate 429 in this slice).
    Queue,
    /// Serve a degraded response (implemented as an immediate 429 in this slice).
    Degrade,
}

/// CORS policy for an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode.
    #[serde(default = "default_sharing")]
    pub sharing: String,
    /// CORS is disabled unless explicitly enabled.
    pub enabled: bool,
    /// Origins allowed to make cross-origin requests.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Methods allowed for cross-origin requests.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers the browser may read from the response.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether the browser may send credentials.
    #[serde(default)]
    pub allow_credentials: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            sharing: default_sharing(),
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// The default protocol when the caller omits it in a route/plugin context.
#[must_use]
pub const fn http_protocol() -> &'static str {
    "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
}

/// The `Upstream` entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// Server-generated GTS identifier.
    #[serde(default)]
    pub id: String,
    /// Owning tenant; taken from the caller's security context, never from the body.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Derived or explicit alias; immutable once set.
    #[serde(default)]
    pub alias: String,
    /// Whether the upstream accepts proxy requests.
    #[serde(default = "crate::domain::default_enabled")]
    pub enabled: bool,
    /// Endpoint pool and protocol.
    #[serde(default)]
    pub server: Server,
    /// Protocol identifier.
    #[serde(default)]
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeadersConfig,
    /// Rate-limit policy.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
    /// CORS policy.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Ordered plugin bindings.
    #[serde(default)]
    pub plugins: Vec<crate::domain::plugin::PluginBinding>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Upstream {
    /// Creates an empty upstream with the platform defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The `(host, port, scheme)` triples of the endpoint pool.
    #[must_use]
    pub fn endpoint_triples(&self) -> Vec<(String, u16, &str)> {
        self.server
            .endpoints
            .iter()
            .map(|e| (e.host.clone(), e.port, e.scheme.as_str()))
            .collect()
    }

    /// The alias this upstream's endpoint set derives, when derivable.
    #[must_use]
    pub fn derived_alias(&self) -> Option<String> {
        alias::derive(&self.endpoint_triples())
    }

    /// Validates every rule that applies to an upstream, returning the first violation.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] describing the violation.
    pub fn validate(&self, _cfg: &OagwConfig) -> Result<(), OagwError> {
        if self.server.endpoints.is_empty() {
            return Err(OagwError::new(
                ErrorKind::ValidationError,
                "server.endpoints must contain at least one endpoint",
            ));
        }

        let first = &self.server.endpoints[0];
        for endpoint in &self.server.endpoints {
            if endpoint.scheme != first.scheme {
                return Err(OagwError::new(
                    ErrorKind::ValidationError,
                    "endpoint pool must share a single scheme",
                ));
            }
            if endpoint.port != first.port {
                return Err(OagwError::new(
                    ErrorKind::ValidationError,
                    "endpoint pool must share a single port",
                ));
            }
            alias::validate_host(&endpoint.host)?;
            if endpoint.port == 0 {
                return Err(OagwError::new(
                    ErrorKind::ValidationError,
                    "endpoint port must be between 1 and 65535",
                ));
            }
            if !SCHEMES.contains(&endpoint.scheme.as_str()) {
                return Err(OagwError::new(
                    ErrorKind::ValidationError,
                    format!("endpoint scheme `{}` is not supported", endpoint.scheme),
                ));
            }
            // Whether a plaintext connection is actually opened is a data-plane decision,
            // taken at proxy time; the scheme itself is always legal here.
        }

        if !PROTOCOLS.contains(&self.protocol.as_str()) {
            return Err(OagwError::new(
                ErrorKind::ValidationError,
                format!("protocol `{}` is not a supported protocol", self.protocol),
            ));
        }

        alias::validate_alias(&alias::normalize(&self.alias))?;
        for tag in &self.tags {
            alias::validate_tag(tag)?;
        }

        if let Some(cors) = &self.cors {
            validate_cors(cors)?;
        }

        crate::domain::plugin::validate_bindings(&self.plugins, crate::domain::plugin::Stage::Upstream)?;

        Ok(())
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: Uuid::nil(),
            alias: String::new(),
            enabled: true,
            server: Server { endpoints: Vec::new() },
            protocol: String::new(),
            auth: None,
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: Vec::new(),
            tags: Vec::new(),
        }
    }
}

/// Validate a CORS configuration.
///
/// # Errors
///
/// Returns a validation error when `allow_credentials` is combined with a wildcard
/// origin, or the configuration is otherwise malformed.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    let wildcard = cors
        .allowed_origins
        .iter()
        .any(|o| o == "*");
    if cors.allow_credentials && wildcard {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "cors.allow_credentials cannot be combined with a wildcard origin",
        ));
    }
    if cors.enabled && cors.allowed_origins.is_empty() {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "cors.enabled requires at least one allowed origin",
        ));
    }
    for origin in &cors.allowed_origins {
        if !(origin == "*" || origin.starts_with("http://") || origin.starts_with("https://")) {
            return Err(OagwError::new(
                ErrorKind::ValidationError,
                format!("cors allowed origin `{origin}` is not a valid origin"),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "upstream_tests.rs"]
mod tests;
