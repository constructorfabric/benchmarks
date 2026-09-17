// Created: 2026-09-03 by Constructor Tech
//! Domain models for upstreams, routes and plugins.
//!
//! The wire shape mirrors `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`. `http` is an additional endpoint
//! scheme accepted when `OagwConfig::allow_http_upstream` is set, as
//! required by that configuration flag.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};

/// Default port for a scheme when the endpoint omits `port`.
#[must_use]
pub fn default_port(scheme: EndpointScheme) -> u16 {
    if matches!(scheme, EndpointScheme::Http) {
        80
    } else {
        443
    }
}

/// Timestamp in milliseconds since the Unix epoch.
#[must_use]
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| {
            i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Endpoint transport scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP. Only accepted when `allow_http_upstream` is enabled.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC.
    Grpc,
}

impl EndpointScheme {
    /// Whether the scheme dials over TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http)
    }
}

/// A single dial target of an upstream pool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// Transport scheme.
    pub scheme: EndpointScheme,
    /// Hostname or IP literal.
    pub host: String,
    /// Port. Defaults to 80 for `http` and 443 otherwise.
    #[serde(default)]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port, applying the scheme default when omitted.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port.unwrap_or_else(|| default_port(self.scheme))
    }
}

/// The endpoint pool of an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoints forming a load-balancing pool.
    pub endpoints: Vec<Endpoint>,
}

/// Hierarchical sharing mode of a configuration section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Visible; descendants may override.
    Inherit,
    /// Visible; descendants may not override.
    Enforce,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (default) — allows bursts.
    #[default]
    TokenBucket,
    /// Sliding window — prevents boundary bursts.
    SlidingWindow,
}

/// Time window of a sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateWindow {
    /// One second (default).
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

/// Counter scope of a rate limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One process-wide counter.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per matched route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with 429 and `Retry-After` (default).
    #[default]
    Reject,
    /// Queue the request (not implemented in this build).
    Queue,
    /// Degrade the request (not implemented in this build).
    Degrade,
}

/// Sustained refill rate of a token bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window length. Defaults to `second`.
    #[serde(default)]
    pub window: RateWindow,
}

/// Burst capacity override of a token bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BurstConfig {
    /// Maximum bucket size; defaults to `sustained.rate`.
    pub capacity: u64,
}

/// Dual-rate limiting configuration (`ADR/0003-rate-limiting.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm. Defaults to `token_bucket`.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained refill rate.
    pub sustained: SustainedRate,
    /// Burst capacity override.
    #[serde(default)]
    pub burst: Option<BurstConfig>,
    /// Counter scope. Defaults to `tenant`.
    #[serde(default)]
    pub scope: RateScope,
    /// Exhaustion strategy. Defaults to `reject`.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request. Defaults to 1.
    #[serde(default)]
    pub cost: Option<u64>,
}

impl RateLimitConfig {
    /// Bucket capacity: `burst.capacity` or the sustained rate.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.burst.as_ref().map_or(self.sustained.rate, |b| b.capacity)
    }

    /// Tokens consumed by one request.
    #[must_use]
    pub fn cost(&self) -> u64 {
        self.cost.unwrap_or(1)
    }

    /// The sustained refill rate expressed in tokens per second.
    #[must_use]
    pub fn rate_per_second(&self) -> f64 {
        let seconds = match self.sustained.window {
            RateWindow::Second => 1.0,
            RateWindow::Minute => 60.0,
            RateWindow::Hour => 3600.0,
            RateWindow::Day => 86_400.0,
        };
        f64::from(u32::try_from(self.sustained.rate).unwrap_or(u32::MAX)) / seconds
    }
}

/// Request-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to set, overwriting any inbound value.
    #[serde(default)]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    /// Headers to add without overwriting.
    #[serde(default)]
    pub add: Option<std::collections::BTreeMap<String, String>>,
    /// Inbound header names to drop before forwarding.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: Option<PassthroughMode>,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// Response-side header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to set on the client response.
    #[serde(default)]
    pub set: Option<std::collections::BTreeMap<String, String>>,
    /// Headers to append to the client response.
    #[serde(default)]
    pub add: Option<std::collections::BTreeMap<String, String>>,
    /// Upstream response header names to strip.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
}

/// Passthrough policy for inbound request headers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward nothing but the allowlist (default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything that is not stripped by the gateway.
    All,
}

/// Header transformation rules of an upstream (`headers` field).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(default)]
    pub request: Option<RequestHeaderRules>,
    /// Response-side rules.
    #[serde(default)]
    pub response: Option<ResponseHeaderRules>,
}

/// CORS configuration of an upstream or route (`ADR/0004-cors.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is active. Required.
    pub enabled: bool,
    /// Allowed origins; `*` allows any origin.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
    /// Allowed methods; defaults to `GET` and `POST`.
    #[serde(default)]
    pub allowed_methods: Option<Vec<String>>,
    /// Headers exposed to browsers beyond the CORS-safelisted set.
    #[serde(default)]
    pub expose_headers: Option<Vec<String>>,
    /// Whether credentialed requests are allowed.
    #[serde(default)]
    pub allow_credentials: Option<bool>,
}

/// A plugin reference inside `plugins.items`.
///
/// Both the bare-string form of `upstream.v1.schema.json` and the binding
/// object form (`plugin_ref` + `config`) of `ADR/0009-required-headers-guard-plugin.md`
/// are accepted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum PluginItem {
    /// Bare plugin identifier (GTS id or custom plugin UUID).
    Ref(String),
    /// Plugin identifier with a binding-scoped configuration override.
    Binding {
        /// Canonical plugin identifier.
        plugin_ref: String,
        /// Configuration merged with the plugin's own configuration.
        #[serde(default)]
        config: Option<serde_json::Value>,
    },
}

impl PluginItem {
    /// The canonical plugin identifier.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Ref(value) => value,
            Self::Binding { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The binding-scoped configuration, when present.
    #[must_use]
    pub fn config(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Ref(_) => None,
            Self::Binding { config, .. } => config.as_ref(),
        }
    }
}

/// Ordered plugin chain of an upstream or route.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugins applied in order; upstream plugins run before route plugins.
    #[serde(default)]
    pub items: Vec<PluginItem>,
}

/// Authentication configuration of an upstream (`auth` field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Auth plugin identifier (`gts.cf.core.oagw.auth_plugin.v1~...`).
    #[serde(default, rename = "type")]
    pub auth_type: Option<String>,
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration, including `cred://` secret references.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// The subset of plugin kinds accepted by the plugin CRUD surface.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginType {
    /// Credential injection.
    #[default]
    Auth,
    /// Validation / policy enforcement.
    Guard,
    /// Request/response mutation.
    Transform,
}

impl PluginType {
    /// The GTS base type of this plugin type.
    #[must_use]
    pub const fn gts_base(self) -> &'static str {
        match self {
            Self::Auth => crate::gts::AUTH_PLUGIN_TYPE,
            Self::Guard => crate::gts::GUARD_PLUGIN_TYPE,
            Self::Transform => crate::gts::TRANSFORM_PLUGIN_TYPE,
        }
    }
}

/// HTTP methods accepted in an HTTP route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
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
    /// The wire name of the method.
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

/// How the proxy path suffix is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the suffix to `path` (default).
    #[default]
    Append,
}

/// HTTP matching rules of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods accepted by this route.
    pub methods: Vec<HttpMethod>,
    /// Path prefix matched against the proxy path.
    pub path: String,
    /// Allowed query parameter names; empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated. Defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules of a route. Catalogued; not proxied in this build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped matching rules of a route; exactly one of `http`/`grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename = "RouteMatch")]
pub struct RouteMatch {
    /// HTTP match rules.
    #[serde(default)]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default)]
    pub grpc: Option<GrpcMatch>,
}

/// A stored upstream configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key used in proxy URLs; normalized to ASCII lowercase.
    pub alias: String,
    /// Whether this upstream accepts proxy traffic.
    #[serde(default = "crate::model::true_default")]
    pub enabled: bool,
    /// Discovery tags, unioned additively across the tenant hierarchy.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol selector.
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Upstream-level plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Upstream-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp (milliseconds since the Unix epoch).
    pub created_at: i64,
    /// Last modification timestamp.
    pub updated_at: i64,
}

/// Request body of `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct UpstreamInput {
    /// Explicit alias; required for non-derivable endpoint pools.
    #[serde(default)]
    pub alias: Option<String>,
    /// Whether the upstream accepts proxy traffic. Defaults to `true`.
    #[serde(default = "crate::model::true_default")]
    pub enabled: bool,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Upstream protocol selector.
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Upstream-level plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Upstream-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// A stored route definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream; immutable after creation.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    #[serde(default = "crate::model::true_default")]
    pub enabled: bool,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match")]
    pub r#match: RouteMatch,
    /// Route-level plugin chain, appended after the upstream chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Route-level rate limit, merged with `min()` against the upstream's.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp (milliseconds since the Unix epoch).
    pub created_at: i64,
    /// Last modification timestamp.
    pub updated_at: i64,
}

/// Request body of `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`.
///
/// `upstream_id` is accepted on create only; it is immutable on replace.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RouteInput {
    /// Owning upstream. Create only.
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    /// Whether the route participates in matching. Defaults to `true`.
    #[serde(default = "crate::model::true_default")]
    pub enabled: bool,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Match rules.
    #[serde(rename = "match", default)]
    pub r#match: Option<RouteMatch>,
    /// Route-level plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Route-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// A tenant-defined declarative plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginRecord {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin name; unique per tenant.
    pub name: String,
    /// Plugin type.
    pub plugin_type: PluginType,
    /// Declarative plugin configuration.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// Creation timestamp.
    pub created_at: i64,
    /// Timestamp of the last proxy request that resolved this plugin.
    #[serde(default)]
    pub last_used_at: Option<i64>,
    /// Timestamp after which an unlinked plugin may be collected.
    #[serde(default)]
    pub gc_eligible_at: Option<i64>,
}

/// Request body of `POST /oagw/v1/plugins`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginInput {
    /// Plugin name; unique per tenant.
    pub name: String,
    /// Plugin type (`auth`, `guard` or `transform`).
    pub plugin_type: PluginType,
    /// Declarative plugin configuration.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// Renders an epoch-milliseconds value as an RFC 3339 timestamp.
#[must_use]
pub fn rfc3339(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let millis_part = millis.rem_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis_part:03}Z")
}

/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

fn true_default() -> bool {
    true
}

/// Validation context carrying the switches that influence which endpoint
/// schemes are legal.
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidationContext {
    /// Whether `http` endpoints may be created and dialled.
    pub allow_http_upstream: bool,
}

impl ValidationContext {
    /// The accepted endpoint scheme set for this context.
    #[must_use]
    pub fn accepts_scheme(self, scheme: EndpointScheme) -> bool {
        if matches!(scheme, EndpointScheme::Http) {
            self.allow_http_upstream
        } else {
            true
        }
    }

    /// The human-readable scheme allowlist for error messages.
    #[must_use]
    pub fn allowed_schemes(self) -> &'static str {
        if self.allow_http_upstream {
            "http, https, wss, wt, grpc"
        } else {
            "https, wss, wt, grpc"
        }
    }
}

/// Validates an endpoint pool: scheme allowlist, hostname syntax and pool
/// homogeneity.
///
/// Returns the normalized endpoints in input order.
///
/// # Errors
/// Returns a 400 `ValidationError` for empty pools, unknown schemes, invalid
/// hostnames or heterogeneous pools.
pub fn validate_endpoints(
    endpoints: &[Endpoint],
    vctx: ValidationContext,
) -> Result<Vec<Endpoint>, OagwError> {
    if endpoints.is_empty() {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "server.endpoints must contain at least one endpoint",
        ));
    }
    let mut validated = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if !vctx.accepts_scheme(endpoint.scheme) {
            return Err(OagwError::new(
                ErrorKind::Validation,
                format!(
                    "endpoint scheme '{}' is not allowed; permitted schemes: {}",
                    serde_json::to_string(&endpoint.scheme).unwrap_or_default(),
                    vctx.allowed_schemes()
                ),
            ));
        }
        let host = crate::alias::validate_host(&endpoint.host)?;
        validated.push(Endpoint {
            scheme: endpoint.scheme,
            host,
            port: endpoint.port,
        });
    }
    let (first_scheme, first_port) = (validated[0].scheme, validated[0].port());
    let heterogeneous = validated
        .iter()
        .any(|e| e.scheme != first_scheme || e.port() != first_port);
    if heterogeneous {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "all endpoints in a pool must share the same scheme and port",
        ));
    }
    Ok(validated)
}

/// Validates a tag list against `^[a-z0-9_-]+$`.
///
/// # Errors
/// Returns a 400 `ValidationError` for tags with characters outside the
/// permitted set.
pub fn validate_tags(tags: &[String]) -> Result<(), OagwError> {
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.len() <= 64
            && tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !valid {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "tags must be lowercase ASCII letters, digits, '_' or '-'",
            ));
        }
    }
    Ok(())
}

/// Validates a rate-limit configuration.
///
/// # Errors
/// Returns a 400 `ValidationError` for non-positive rates or capacities.
pub fn validate_rate_limit(limit: &RateLimitConfig) -> Result<(), OagwError> {
    if limit.sustained.rate == 0 {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if let Some(burst) = &limit.burst
        && burst.capacity == 0 {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "rate_limit.burst.capacity must be at least 1",
            ));
        }
    if let Some(cost) = limit.cost
        && cost == 0 {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "rate_limit.cost must be at least 1",
            ));
        }
    Ok(())
}

/// Validates a CORS configuration.
///
/// # Errors
/// Returns a 400 `ValidationError` when `allow_credentials` is combined with
/// the wildcard origin.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    if cors.allow_credentials.unwrap_or(false)
        && cors
            .allowed_origins
            .as_ref()
            .is_some_and(|origins| origins.iter().any(|o| o == "*"))
    {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "cors.allow_credentials cannot be combined with allowed_origins ['*']",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_port_defaults_follow_scheme() {
        let http = Endpoint {
            scheme: EndpointScheme::Http,
            host: "a".to_owned(),
            port: None,
        };
        let https = Endpoint {
            scheme: EndpointScheme::Https,
            host: "a".to_owned(),
            port: None,
        };
        assert_eq!(http.port(), 80);
        assert_eq!(https.port(), 443);
    }

    #[test]
    fn http_scheme_is_rejected_unless_allowed() {
        let endpoints = [Endpoint {
            scheme: EndpointScheme::Http,
            host: "localhost".to_owned(),
            port: Some(8080),
        }];
        assert!(validate_endpoints(&endpoints, ValidationContext::default()).is_err());
        assert!(validate_endpoints(
            &endpoints,
            ValidationContext {
                allow_http_upstream: true
            }
        )
        .is_ok());
    }

    #[test]
    fn homogeneous_pool_is_required() {
        let endpoints = [
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "a.example.com".to_owned(),
                port: Some(443),
            },
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "b.example.com".to_owned(),
                port: Some(8443),
            },
        ];
        assert!(validate_endpoints(&endpoints, ValidationContext::default()).is_err());
    }

    #[test]
    fn cors_wildcard_with_credentials_is_rejected() {
        let cors = CorsConfig {
            sharing: SharingMode::default(),
            enabled: true,
            allowed_origins: Some(vec!["*".to_owned()]),
            allowed_methods: None,
            expose_headers: None,
            allow_credentials: Some(true),
        };
        assert!(validate_cors(&cors).is_err());
    }

    #[test]
    fn rate_limit_validation_rejects_zero_rate() {
        let limit = RateLimitConfig {
            sharing: SharingMode::default(),
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 0,
                window: RateWindow::Second,
            },
            burst: None,
            scope: RateScope::default(),
            strategy: RateStrategy::default(),
            cost: None,
        };
        assert!(validate_rate_limit(&limit).is_err());
    }

    #[test]
    fn rfc3339_formats_known_epoch() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_770_000_000_123), "2026-02-02T02:40:00.123Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn http_methods_round_trip() {
        assert_eq!(HttpMethod::parse("PATCH"), Some(HttpMethod::Patch));
        assert_eq!(HttpMethod::parse("GET"), Some(HttpMethod::Get));
        assert_eq!(HttpMethod::parse("OPTIONS"), None);
        assert_eq!(HttpMethod::Get.as_str(), "GET");
    }
}
