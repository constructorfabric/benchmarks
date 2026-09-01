// Created: 2026-08-29 by Constructor Tech
//! Domain model for the outbound API gateway control plane.
//!
//! The wire shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` exactly, including
//! `additionalProperties: false` (unknown fields are rejected) and the
//! documented defaults.

use std::collections::BTreeSet;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::error::OagwError;
use super::plugin;

/// Hard request-body limit (DESIGN constraint `cpt-cf-oagw-constraint-body-limit`).
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Hierarchical sharing mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

/// Upstream endpoint (`scheme://host:port`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// `https` | `wss` | `wt` | `grpc` (and `http` for explicitly permitted
    /// plaintext upstreams).
    pub scheme: String,
    /// RFC 1123 hostname, IPv4 or IPv6 literal.
    pub host: String,
    /// Defaults to `443`.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    443
}

impl Endpoint {
    /// Standard port for the endpoint scheme (HTTP 80, everything else 443).
    #[must_use]
    pub fn standard_port(&self) -> u16 {
        if self.scheme == "http" { 80 } else { 443 }
    }

    /// `true` when the host is an IPv4 / IPv6 literal.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        std::net::IpAddr::from_str(&self.host).is_ok()
    }

    /// Normalized host for comparisons (ASCII lowercase, trailing dot stripped).
    #[must_use]
    pub fn normalized_host(&self) -> String {
        normalize_host(&self.host)
    }
}

/// Normalize a host or alias: ASCII lowercase, trim, strip trailing dots.
#[must_use]
pub fn normalize_host(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Validate a host / alias per RFC 1123: labels of 1..=63 characters, total
/// length <= 253, only ASCII alphanumerics and hyphens inside labels, labels
/// must not start or end with a hyphen, and an optional single `:port` suffix.
///
/// A trailing dot (FQDN notation) is tolerated and stripped.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the value is empty, longer than 253
/// characters, contains an invalid character, or a label is empty / longer than
/// 63 characters / starts or ends with a hyphen.
pub fn validate_hostname_like(value: &str) -> Result<String, OagwError> {
    let normalized = normalize_host(value);
    if normalized.is_empty() {
        return Err(OagwError::Validation(
            "alias/host must not be empty".to_owned(),
        ));
    }
    if normalized.len() > 253 {
        return Err(OagwError::Validation(
            "alias/host exceeds the 253 character limit".to_owned(),
        ));
    }
    let (host_part, port_part) = split_port(&normalized);
    if let Some(port) = port_part {
        if host_part.is_empty() {
            return Err(OagwError::Validation(
                "alias/host must not start with ':'".to_owned(),
            ));
        }
        if host_part.contains(':') {
            return Err(OagwError::Validation(
                "alias/host must be an RFC 1123 hostname optionally followed by ':port' \
                 (IPv6 literals are not valid aliases)"
                    .to_owned(),
            ));
        }
        if port > 65_535 {
            return Err(OagwError::Validation(format!(
                "port '{port}' in alias/host is out of range"
            )));
        }
    }
    if host_part.is_empty() {
        return Err(OagwError::Validation(
            "alias/host must not be empty".to_owned(),
        ));
    }
    for label in host_part.split('.') {
        validate_label(label)?;
    }
    Ok(normalized)
}

fn validate_label(label: &str) -> Result<(), OagwError> {
    if label.is_empty() {
        return Err(OagwError::Validation(
            "alias/host must not contain empty labels".to_owned(),
        ));
    }
    if label.len() > 63 {
        return Err(OagwError::Validation(
            "alias/host label exceeds 63 characters".to_owned(),
        ));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(OagwError::Validation(
            "alias/host labels must not start or end with '-'".to_owned(),
        ));
    }
    if !label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(OagwError::Validation(format!(
            "alias/host label '{label}' contains characters outside [a-z0-9-]"
        )));
    }
    Ok(())
}

/// Split `host:port` into `(host, Some(port))`, keeping values whose tail is
/// not a decimal port intact.
fn split_port(value: &str) -> (&str, Option<u32>) {
    match value.rsplit_once(':') {
        Some((host, port)) => port
            .parse::<u32>()
            .map_or((value, None), |parsed| (host, Some(parsed))),
        None => (value, None),
    }
}

/// Server configuration: a pool of endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// One or more endpoints. All endpoints share `scheme` and `port`.
    pub endpoints: Vec<Endpoint>,
}

/// Outbound authentication plugin binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    /// Plugin GTS identifier (`gts.cf.core.oagw.auth_plugin.v1~<instance>`).
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// Plugin configuration object.
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// Header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Hierarchical sharing mode (`private` by default, matching the schema
    /// which does not expose the field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<Sharing>,
    /// Rules applied to the request leg.
    #[serde(default)]
    pub request: RequestHeaders,
    /// Rules applied to the response leg.
    #[serde(default)]
    pub response: ResponseHeaders,
}

/// Request header rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// Overwrite if present.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Append, duplicates allowed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to drop from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// `none` | `allowlist` | `all`.
    #[serde(default)]
    pub passthrough: Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Which inbound request headers are forwarded upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Passthrough {
    /// Forward none (default).
    #[default]
    None,
    /// Forward only the allowlisted names.
    Allowlist,
    /// Forward everything except routing and hop-by-hop headers.
    All,
}

/// Response header rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// Overwrite if present.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Append, duplicates allowed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Names stripped from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Plugin chain binding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PluginsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// GTS ids of built-ins or UUIDs of custom plugins, optionally carrying
    /// that plugin's configuration (ADR-0009).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginItem>,
}

/// One entry of a plugin chain: a bare reference or a reference with config.
///
/// ADR-0009 binds `required_headers.v1` as
/// `{"plugin_ref": "…required_headers.v1", "config": {…}}`; the JSON Schema's
/// `oneOf` also admits the bare string form used by plugins that need no
/// configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginItem {
    /// A plugin reference without configuration.
    Reference(String),
    /// A plugin reference with its per-binding configuration.
    Configured {
        /// GTS id or UUID of the plugin.
        plugin_ref: String,
        /// Configuration handed to the plugin at request time.
        #[serde(default)]
        config: serde_json::Value,
    },
}

impl PluginItem {
    /// The plugin reference this entry binds.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            Self::Reference(value)
            | Self::Configured {
                plugin_ref: value, ..
            } => value,
        }
    }

    /// The configuration this entry carries, `null` when it has none.
    #[must_use]
    pub fn config(&self) -> serde_json::Value {
        match self {
            Self::Reference(_) => serde_json::Value::Null,
            Self::Configured { config, .. } => config.clone(),
        }
    }
}

/// Sustained rate definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sustained {
    /// Tokens replenished per `window`.
    pub rate: u32,
    /// `second` | `minute` | `hour` | `day`.
    #[serde(default)]
    pub window: RateWindow,
}

impl Default for Sustained {
    fn default() -> Self {
        Self {
            rate: 1,
            window: RateWindow::Second,
        }
    }
}

/// Time window for the sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
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

impl RateWindow {
    /// Window length in seconds.
    #[must_use]
    pub fn seconds(self) -> u64 {
        match self {
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Burst capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    /// Bucket capacity; defaults to `sustained.rate`.
    pub capacity: u32,
}

/// Token budget allocation across the hierarchy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// `unlimited` | `allocated` | `shared`.
    #[serde(default)]
    pub mode: BudgetMode,
    /// Total tokens per window when the mode is not `unlimited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// Overcommit ratio (default 1.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overcommit_ratio: Option<f64>,
}

/// Budget mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BudgetMode {
    /// No budget cap (default).
    #[default]
    Unlimited,
    /// Explicitly allocated.
    Allocated,
    /// Shared across the hierarchy.
    Shared,
}

/// Rate limiting configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// `token_bucket` | `sliding_window`.
    #[serde(default)]
    pub algorithm: RateAlgorithm,
    /// Sustained rate.
    pub sustained: Sustained,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    /// Budget allocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    /// Counter scope; default `tenant`.
    #[serde(default)]
    pub scope: RateScope,
    /// `reject` | `queue` | `degrade`.
    #[serde(default)]
    pub strategy: RateStrategy,
    /// Tokens consumed per request; default 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u32>,
    /// Emit `X-RateLimit-*` headers; default `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<bool>,
}

impl RateLimitConfig {
    /// Bucket capacity: `burst.capacity` when set, else `sustained.rate`.
    #[must_use]
    pub fn capacity(&self) -> u32 {
        self.burst.map_or(self.sustained.rate, |b| b.capacity)
    }

    /// Tokens consumed per request (default 1).
    #[must_use]
    pub fn cost(&self) -> u32 {
        self.cost.unwrap_or(1)
    }

    /// `X-RateLimit-*` headers enabled (default `true`).
    #[must_use]
    pub fn response_headers(&self) -> bool {
        self.response_headers.unwrap_or(true)
    }
}

/// Rate limit algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RateAlgorithm {
    /// Token bucket (default).
    #[default]
    TokenBucket,
    /// Sliding window.
    SlidingWindow,
}

/// Rate limit counter scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateScope {
    /// One bucket shared by every request.
    Global,
    /// One bucket per tenant (default).
    #[default]
    Tenant,
    /// One bucket per authenticated subject.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RateStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Serve a degraded response.
    Degrade,
}

/// CORS configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Hierarchical sharing mode.
    #[serde(default)]
    pub sharing: Sharing,
    /// CORS is off unless explicitly enabled.
    pub enabled: bool,
    /// Origins allowed; `["*"]` allows any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Methods allowed; default `["GET", "POST"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_methods: Option<Vec<String>>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials; requires specific origins.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_credentials: bool,
}

impl CorsConfig {
    /// Effective methods: the configured set or the `GET`/`POST` default.
    #[must_use]
    pub fn methods(&self) -> Vec<String> {
        self.allowed_methods
            .clone()
            .unwrap_or_else(|| vec!["GET".to_owned(), "POST".to_owned()])
    }
}

/// Protocol-scoped inbound match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP match; exactly one of `http` / `grpc` must be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match; exactly one of `http` / `grpc` must be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// HTTP match rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Method allowlist (minimum one).
    pub methods: Vec<String>,
    /// Path pattern used as a prefix.
    pub path: String,
    /// Only these query parameters are forwarded; empty forwards none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// `disabled` | `append` (default `append`).
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// How `/{path_suffix}` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Append the suffix to `match.path` (default).
    #[default]
    Append,
    /// A non-empty suffix is rejected with `400 RouteError`.
    Disabled,
}

/// gRPC match rules (catalogued; no gRPC proxy code path is reachable in MVP).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

fn default_true() -> bool {
    true
}

/// Creation payload for an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCreate {
    /// Enabled flag; default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit alias; required for non-derivable endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// Stored upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Upstream {
    /// System-generated id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Normalized routing key (immutable after creation).
    pub alias: String,
    /// `true` when the alias was derived from the endpoints.
    pub alias_derived: bool,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
    /// Stored upstream payload.
    #[serde(flatten)]
    pub spec: UpstreamCreate,
}

/// Creation payload for a route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteCreate {
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Enabled flag; default `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

/// Stored route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Route {
    /// System-generated id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
    /// Stored route payload.
    #[serde(flatten)]
    pub spec: RouteCreate,
}

/// Custom plugin definition (Starlark source).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginDefinition {
    /// System-generated id (UUID).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// `auth_plugin` | `guard_plugin` | `transform_plugin`.
    pub plugin_type: String,
    /// Human readable name.
    pub name: String,
    /// Sandboxed Starlark source.
    pub source_code: String,
    /// Optional JSON schema for the plugin config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
    /// Last update timestamp (RFC 3339).
    pub updated_at: String,
}

/// Plugin creation payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCreate {
    /// `auth_plugin` | `guard_plugin` | `transform_plugin`.
    pub plugin_type: String,
    /// Human readable name.
    pub name: String,
    /// Sandboxed Starlark source.
    pub source_code: String,
    /// Optional JSON schema for the plugin config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}

/// GTS plugin-kind prefixes accepted for custom plugins.
const PLUGIN_KINDS: [&str; 3] = ["auth_plugin", "guard_plugin", "transform_plugin"];

/// Validate a tag against `^[a-z0-9_-]+$`.
fn validate_tag(tag: &str) -> Result<(), OagwError> {
    let valid = !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(OagwError::Validation(format!(
            "tag '{tag}' must match ^[a-z0-9_-]+$"
        )))
    }
}

/// Validate a set of tags (also rejects duplicates).
fn validate_tags(tags: &[String]) -> Result<(), OagwError> {
    for tag in tags {
        validate_tag(tag)?;
    }
    let unique: BTreeSet<&String> = tags.iter().collect();
    if unique.len() != tags.len() {
        return Err(OagwError::Validation(
            "tags must not contain duplicates".to_owned(),
        ));
    }
    Ok(())
}

/// Validate a plugin reference: a full GTS identifier or a bare UUID.
fn validate_plugin_ref(value: &str) -> Result<(), OagwError> {
    if Uuid::parse_str(value).is_ok() {
        return Ok(());
    }
    validate_gts_id(value, "plugin reference")
}

/// Validate a GTS identifier of the form `gts.<path>.<type>.v1~<instance>`.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the value is not a GTS identifier.
pub fn validate_gts_id(value: &str, what: &str) -> Result<(), OagwError> {
    if !value.starts_with("gts.") {
        return Err(OagwError::Validation(format!(
            "{what} '{value}' must be a GTS identifier"
        )));
    }
    let Some((_type_part, instance)) = value.split_once('~') else {
        return Err(OagwError::Validation(format!(
            "{what} '{value}' must contain '~'"
        )));
    };
    if instance.is_empty() {
        return Err(OagwError::Validation(format!(
            "{what} '{value}' must have a non-empty instance part"
        )));
    }
    Ok(())
}

/// Validate the upstream creation payload.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] describing the first violation found.
pub fn validate_upstream_create(spec: &UpstreamCreate) -> Result<(), OagwError> {
    if spec.server.endpoints.is_empty() {
        return Err(OagwError::Validation(
            "server.endpoints must contain at least one endpoint".to_owned(),
        ));
    }
    validate_protocol(&spec.protocol)?;
    validate_tags(&spec.tags)?;
    for endpoint in &spec.server.endpoints {
        validate_scheme(&endpoint.scheme)?;
        validate_hostname_like(&endpoint.host)?;
    }
    validate_endpoint_homogeneity(&spec.server.endpoints)?;
    if let Some(auth) = &spec.auth {
        validate_auth(auth)?;
    }
    if let Some(cors) = &spec.cors {
        validate_cors(cors)?;
    }
    if let Some(rate) = &spec.rate_limit {
        validate_rate_limit(rate)?;
    }
    if let Some(plugins) = &spec.plugins {
        for item in &plugins.items {
            validate_plugin_ref(item.reference())?;
        }
    }
    Ok(())
}

fn validate_endpoint_homogeneity(endpoints: &[Endpoint]) -> Result<(), OagwError> {
    let schemes: BTreeSet<&str> = endpoints.iter().map(|e| e.scheme.as_str()).collect();
    if schemes.len() > 1 {
        return Err(OagwError::Validation(
            "all endpoints of an upstream must use the same scheme".to_owned(),
        ));
    }
    let ports: BTreeSet<u16> = endpoints.iter().map(|e| e.port).collect();
    if ports.len() > 1 {
        return Err(OagwError::Validation(
            "all endpoints of an upstream must use the same port".to_owned(),
        ));
    }
    Ok(())
}

fn validate_auth(auth: &AuthConfig) -> Result<(), OagwError> {
    validate_gts_id(&auth.plugin_type, "auth.type")?;
    let instance = plugin_instance(&auth.plugin_type);
    if Uuid::parse_str(instance).is_ok() {
        return Ok(());
    }
    if plugin::is_implementable_auth_plugin(instance) {
        return Ok(());
    }
    // A reserved catalog-only id has no implementation: as the upstream's only
    // credential source it could never authenticate, so it is refused here
    // rather than failing with `503 PluginNotFound` on every request.
    if plugin::is_catalog_only_plugin(instance) {
        return Err(OagwError::Validation(format!(
            "auth.type '{instance}' is a catalog identifier with no implementation"
        )));
    }
    Err(OagwError::Validation(format!(
        "auth.type '{instance}' is not a known auth plugin"
    )))
}

/// Validate the route creation payload.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the match rule is missing, ambiguous,
/// empty, or when tags / plugin refs are malformed.
pub fn validate_route_create(spec: &RouteCreate) -> Result<(), OagwError> {
    validate_tags(&spec.tags)?;
    match (
        spec.match_config.http.as_ref(),
        spec.match_config.grpc.as_ref(),
    ) {
        (Some(http), None) => validate_http_match(http),
        (None, Some(grpc)) => validate_grpc_match(grpc),
        (Some(_), Some(_)) | (None, None) => Err(OagwError::Validation(
            "route match must define exactly one of 'http' or 'grpc'".to_owned(),
        )),
    }?;
    if let Some(cors) = &spec.cors {
        validate_cors(cors)?;
    }
    if let Some(rate) = &spec.rate_limit {
        validate_rate_limit(rate)?;
    }
    if let Some(plugins) = &spec.plugins {
        for item in &plugins.items {
            validate_plugin_ref(item.reference())?;
        }
    }
    Ok(())
}

/// Validate a custom plugin payload.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the plugin kind is unknown.
pub fn validate_plugin_create(spec: &PluginCreate) -> Result<(), OagwError> {
    if !PLUGIN_KINDS.contains(&spec.plugin_type.as_str()) {
        return Err(OagwError::Validation(format!(
            "plugin_type '{}' must be one of {PLUGIN_KINDS:?}",
            spec.plugin_type
        )));
    }
    if spec.name.trim().is_empty() {
        return Err(OagwError::Validation("name must not be empty".to_owned()));
    }
    Ok(())
}

fn validate_protocol(protocol: &str) -> Result<(), OagwError> {
    if protocol == PROTOCOL_HTTP || protocol == PROTOCOL_GRPC {
        Ok(())
    } else {
        Err(OagwError::Validation(format!(
            "protocol '{protocol}' must be one of '{PROTOCOL_HTTP}' or '{PROTOCOL_GRPC}'"
        )))
    }
}

fn validate_scheme(scheme: &str) -> Result<(), OagwError> {
    if matches!(scheme, "https" | "wss" | "wt" | "grpc" | "http") {
        Ok(())
    } else {
        Err(OagwError::Validation(format!(
            "endpoint scheme '{scheme}' must be one of https|wss|wt|grpc"
        )))
    }
}

fn validate_http_match(http: &HttpMatch) -> Result<(), OagwError> {
    if http.methods.is_empty() {
        return Err(OagwError::Validation(
            "match.http.methods must contain at least one method".to_owned(),
        ));
    }
    const ALLOWED: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
    for method in &http.methods {
        if !ALLOWED.contains(&method.as_str()) {
            return Err(OagwError::Validation(format!(
                "match.http.methods entry '{method}' must be one of {ALLOWED:?}"
            )));
        }
    }
    if http.path.is_empty() {
        return Err(OagwError::Validation(
            "match.http.path must not be empty".to_owned(),
        ));
    }
    if !http.path.starts_with('/') {
        return Err(OagwError::Validation(
            "match.http.path must start with '/'".to_owned(),
        ));
    }
    Ok(())
}

fn validate_grpc_match(grpc: &GrpcMatch) -> Result<(), OagwError> {
    if grpc.service.is_empty() || grpc.method.is_empty() {
        return Err(OagwError::Validation(
            "match.grpc.service and match.grpc.method must not be empty".to_owned(),
        ));
    }
    Ok(())
}

fn validate_rate_limit(rate: &RateLimitConfig) -> Result<(), OagwError> {
    if rate.sustained.rate == 0 {
        return Err(OagwError::Validation(
            "rate_limit.sustained.rate must be at least 1".to_owned(),
        ));
    }
    if rate.capacity() == 0 {
        return Err(OagwError::Validation(
            "rate_limit.burst.capacity must be at least 1".to_owned(),
        ));
    }
    if let Some(budget) = &rate.budget
        && budget.mode != BudgetMode::Unlimited
        && budget.total.is_none()
    {
        return Err(OagwError::Validation(
            "rate_limit.budget.total is required when budget.mode is not 'unlimited'".to_owned(),
        ));
    }
    if rate.strategy != RateStrategy::Reject {
        return Err(OagwError::Validation(
            "rate_limit.strategy must be 'reject': the gateway has no queue or degraded              response to serve"
                .to_owned(),
        ));
    }
    if rate.cost() > rate.capacity() {
        return Err(OagwError::Validation(
            "rate_limit.cost must not exceed the burst capacity: a request would never fit              the bucket"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Validate a CORS block, including the `allow_credentials` + `*` rule.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for unknown methods or for
/// `allow_credentials: true` combined with a `*` origin.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    const ALLOWED_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
    if let Some(methods) = &cors.allowed_methods {
        for method in methods {
            if !ALLOWED_METHODS.contains(&method.as_str()) {
                return Err(OagwError::Validation(format!(
                    "cors.allowed_methods entry '{method}' must be one of {ALLOWED_METHODS:?}"
                )));
            }
        }
    }
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(OagwError::Validation(
            "cors.allow_credentials must not be combined with allowed_origins '*'".to_owned(),
        ));
    }
    for origin in &cors.allowed_origins {
        if origin == "*" || origin.starts_with("http://") || origin.starts_with("https://") {
            continue;
        }
        return Err(OagwError::Validation(format!(
            "cors.allowed_origins entry '{origin}' must be '*' or an absolute origin"
        )));
    }
    Ok(())
}

/// Extract the plugin instance part of a GTS identifier (`…~<instance>`).
#[must_use]
pub fn plugin_instance(gts_id: &str) -> &str {
    gts_id.split_once('~').map_or(gts_id, |(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream_json(body: serde_json::Value) -> Result<UpstreamCreate, serde_json::Error> {
        serde_json::from_value(body)
    }

    #[test]
    fn rejects_unknown_fields() {
        let err = upstream_json(serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
            "protocol": PROTOCOL_HTTP,
            "nope": 1
        }))
        .expect_err("unknown field must be rejected");
        assert!(err.to_string().contains("nope"));
    }

    #[test]
    fn rejects_empty_endpoints_and_bad_tags() {
        let err = upstream_json(serde_json::json!({
            "server": { "endpoints": [] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect("unknown-field-free body parses");
        assert!(matches!(
            validate_upstream_create(&err),
            Err(OagwError::Validation(msg)) if msg.contains("endpoints")
        ));

        let bad = upstream_json(serde_json::json!({
            "tags": ["Bad Tag"],
            "server": { "endpoints": [{ "scheme": "https", "host": "a.example.com" }] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect("parses");
        assert!(validate_upstream_create(&bad).is_err());
    }

    #[test]
    fn rejects_bad_protocol_and_scheme() {
        let bad = upstream_json(serde_json::json!({
            "server": { "endpoints": [{ "scheme": "ftp", "host": "a.example.com" }] },
            "protocol": PROTOCOL_HTTP
        }))
        .expect("parses");
        assert!(validate_upstream_create(&bad).is_err());

        let bad_proto = upstream_json(serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "a.example.com" }] },
            "protocol": "gts.cf.core.oagw.protocol.v1~nope"
        }))
        .expect("parses");
        assert!(validate_upstream_create(&bad_proto).is_err());
    }

    #[test]
    fn rejects_cors_credentials_with_wildcard() {
        let cors = CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: None,
            expose_headers: Vec::new(),
            allow_credentials: true,
        };
        assert!(validate_cors(&cors).is_err());
    }

    #[test]
    fn rejects_route_match_ambiguity() {
        let both = serde_json::from_value::<RouteCreate>(serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": {
                "http": { "methods": ["GET"], "path": "/a" },
                "grpc": { "service": "s", "method": "m" }
            }
        }))
        .expect("parses");
        assert!(validate_route_create(&both).is_err());

        let neither = serde_json::from_value::<RouteCreate>(serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": {}
        }))
        .expect("parses");
        assert!(validate_route_create(&neither).is_err());

        let empty_methods = serde_json::from_value::<RouteCreate>(serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": [], "path": "/a" } }
        }))
        .expect("parses");
        assert!(validate_route_create(&empty_methods).is_err());
    }
}
