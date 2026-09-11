// Created: 2026-09-01 by Constructor Tech
//! Core domain types.
//!
//! Shapes follow `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`. Field names are the wire names and
//! defaults mirror the schema defaults, so an omitted field deserializes to
//! the documented value rather than an error.
//!
//! Two deviations from the published schemas are deliberate:
//!
//! * `server.endpoints[].scheme` also accepts `http`, because the gateway
//!   must be able to declare a plaintext upstream; whether such a
//!   connection is actually made is governed by `OagwConfig::
//!   allow_http_upstream` at proxy time, not by the create-time schema.
//! * `Route` carries `enabled` and `priority`, which `DESIGN.md` §3.1 lists
//!   in the class diagram but the route schema omits.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// GTS protocol identifiers accepted on upstreams.
pub mod protocol {
    /// HTTP proxying.
    pub const HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    /// gRPC proxying (Phase 3 — catalogued but not routed).
    pub const GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

    /// `true` for the identifiers with a routable data path.
    #[must_use]
    pub fn is_routable(id: &str) -> bool {
        id == HTTP
    }
}

/// Built-in plugin GTS identifiers.
pub mod builtin_plugins {
    /// No authentication.
    pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// API key injection.
    pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// OAuth2 client credentials, `client_secret_post`.
    pub const AUTH_OAUTH2_FORM: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// OAuth2 client credentials, `client_secret_basic`.
    pub const AUTH_OAUTH2_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// HTTP Basic authentication — catalogued, no backing plugin.
    pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    /// Bearer token injection — catalogued, no backing plugin.
    pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
    /// Required-headers guard.
    pub const GUARD_REQUIRED_HEADERS: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// Timeout guard — core Data Plane logic, catalogued only.
    pub const GUARD_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
    /// CORS guard — core Data Plane logic, catalogued only.
    pub const GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
    /// Request-ID transform.
    pub const TRANSFORM_REQUEST_ID: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
    /// Logging — core Data Plane instrumentation, catalogued only.
    pub const TRANSFORM_LOGGING: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
    /// Metrics — core Data Plane instrumentation, catalogued only.
    pub const TRANSFORM_METRICS: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

    /// `true` when `id` names an auth plugin the gateway can resolve.
    #[must_use]
    pub fn is_resolvable_auth(id: &str) -> bool {
        matches!(
            id,
            AUTH_NOOP | AUTH_APIKEY | AUTH_OAUTH2_FORM | AUTH_OAUTH2_BASIC
        )
    }

    /// `true` when `id` names a guard plugin bindable through
    /// `plugins.items[].plugin_ref`.
    #[must_use]
    pub fn is_bindable_guard(id: &str) -> bool {
        id == GUARD_REQUIRED_HEADERS
    }

    /// `true` when `id` names a transform plugin a registry resolves.
    #[must_use]
    pub fn is_resolvable_transform(id: &str) -> bool {
        id == TRANSFORM_REQUEST_ID
    }

    /// `true` when `id` is one of the reserved catalog-only identifiers.
    #[must_use]
    pub fn is_catalog_only(id: &str) -> bool {
        matches!(
            id,
            AUTH_BASIC
                | AUTH_BEARER
                | GUARD_TIMEOUT
                | GUARD_CORS
                | TRANSFORM_LOGGING
                | TRANSFORM_METRICS
        )
    }
}

/// How a piece of hierarchical configuration flows down the tenant tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; descendants may override.
    Inherit,
    /// Visible; descendants may not override.
    Enforce,
}

/// A single upstream endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// `http`, `https`, `wss`, `wt` or `grpc`.
    pub scheme: String,
    /// Hostname or IP address.
    pub host: String,
    /// Port. Defaults to 443.
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_port() -> u16 {
    443
}

impl Endpoint {
    /// `true` when `host` is an IPv4 or IPv6 literal.
    #[must_use]
    pub fn is_ip(&self) -> bool {
        is_ip_literal(&self.host)
    }

    /// The canonical `host[:port]` form used for derivation and matching.
    #[must_use]
    pub fn authority(&self) -> String {
        if is_standard_port(&self.scheme, self.port) {
            self.host.to_ascii_lowercase()
        } else {
            format!("{}:{}", self.host.to_ascii_lowercase(), self.port)
        }
    }

    /// Only `https` and `wss` negotiate TLS.
    #[must_use]
    pub fn is_secure(&self) -> bool {
        matches!(self.scheme.as_str(), "https" | "wss")
    }
}

/// `true` when `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::Ipv6Addr>().is_ok()
}

/// Ports omitted from a derived alias.
#[must_use]
pub fn is_standard_port(scheme: &str, port: u16) -> bool {
    match scheme {
        "http" => port == 80,
        "https" | "wss" | "wt" | "grpc" => port == 443,
        _ => port == 443,
    }
}

/// One upstream endpoint's network location, resolved at proxy time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Host or IP the connection is opened to.
    pub host: String,
    /// Port the connection is opened to.
    pub port: u16,
    /// Whether TLS is negotiated (`https`/`wss`).
    pub secure: bool,
}

impl Target {
    /// The authority written into the upstream `Host` header.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.port == 443 || self.port == 80 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Which inbound headers survive the transformation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
pub enum Passthrough {
    /// Forward nothing (default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything not stripped as hop-by-hop or routing.
    All,
}

impl Passthrough {
    fn is_default(passthrough: &Self) -> bool {
        *passthrough == Self::None
    }
}

impl SharingMode {
    fn is_default(sharing: &Self) -> bool {
        *sharing == Self::Private
    }
}

/// Set / add / remove / passthrough rules for one direction.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct HeaderRules {
    /// Overwrite if present.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Append, allowing duplicates.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Remove from the inbound set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers are forwarded.
    #[serde(skip_serializing_if = "Passthrough::is_default")]
    pub passthrough: Passthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Header transformation rules (`docs/DESIGN.md` §3.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<HeaderRules>,
    /// Response-side rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<HeaderRules>,
}

/// A plugin reference: a bare identifier (built-in GTS id or custom plugin
/// UUID) or a binding carrying its own config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[derive(utoipa::ToSchema)]
pub enum PluginBinding {
    /// Bare reference — `docs/schemas/upstream.v1.schema.json`.
    Ref(String),
    /// Reference with config — `docs/ADR/0009-required-headers-guard-plugin.md`.
    Bound {
        /// The plugin identifier.
        plugin_ref: String,
        /// Plugin configuration.
        #[serde(default)]
        config: BTreeMap<String, serde_json::Value>,
    },
}

impl PluginBinding {
    /// The plugin identifier this binding names.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Ref(id) => id,
            Self::Bound { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The plugin's own config, when the binding carries one.
    #[must_use]
    pub fn config(&self) -> Option<&BTreeMap<String, serde_json::Value>> {
        match self {
            Self::Ref(_) => None,
            Self::Bound { config, .. } => Some(config),
        }
    }

    /// A config entry rendered as a string, when it holds one.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<String> {
        self.config()
            .and_then(|c| c.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    /// A binding that names a plugin without configuring it.
    #[must_use]
    pub fn ref_only(plugin_ref: &str) -> Self {
        Self::Ref(plugin_ref.to_owned())
    }

    /// A binding that names a plugin and configures it.
    #[must_use]
    pub fn with_config(plugin_ref: &str, config: BTreeMap<String, serde_json::Value>) -> Self {
        Self::Bound {
            plugin_ref: plugin_ref.to_owned(),
            config,
        }
    }
}

/// Plugin chain configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct PluginSet {
    /// Sharing mode for the chain.
    #[serde(skip_serializing_if = "SharingMode::is_default")]
    pub sharing: SharingMode,
    /// Plugins applied in order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

/// Window units for a sustained rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
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
    /// The window length.
    #[must_use]
    pub const fn duration(self) -> Duration {
        match self {
            Self::Second => Duration::from_secs(1),
            Self::Minute => Duration::from_secs(60),
            Self::Hour => Duration::from_secs(3600),
            Self::Day => Duration::from_secs(86_400),
        }
    }
}

/// Sustained rate component (`docs/schemas/upstream.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per `window`.
    pub rate: u64,
    /// Window unit.
    pub window: RateWindow,
}

impl Default for SustainedRate {
    fn default() -> Self {
        Self {
            rate: 1,
            window: RateWindow::Second,
        }
    }
}

/// Burst component: the bucket capacity. Defaults to `sustained.rate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct Burst {
    /// Maximum burst size.
    pub capacity: u64,
}

impl Default for Burst {
    fn default() -> Self {
        Self { capacity: 1 }
    }
}

/// Dual-rate rate-limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct RateLimit {
    /// Sharing mode for hierarchical inheritance.
    pub sharing: SharingMode,
    /// `token_bucket` or `sliding_window`.
    pub algorithm: RateAlgorithm,
    /// Sustained rate (required).
    pub sustained: SustainedRate,
    /// Bucket capacity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub burst: Option<Burst>,
    /// Counter scope.
    pub scope: RateScope,
    /// Behaviour on exhaustion.
    pub strategy: RateStrategy,
    /// Tokens consumed per request.
    pub cost: u64,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
pub enum RateScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant.
    #[default]
    Tenant,
    /// One counter per authenticated subject.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
pub enum RateStrategy {
    /// Reject with 429.
    #[default]
    Reject,
    /// Queue the request (modelled as reject with a longer `Retry-After`).
    Queue,
    /// Serve a degraded response (modelled as reject; no upstream call).
    Degrade,
}

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
pub enum RateAlgorithm {
    /// Token bucket — allows bursts.
    #[default]
    TokenBucket,
    /// Sliding window — no boundary burst.
    SlidingWindow,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: SustainedRate::default(),
            burst: None,
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost: 1,
        }
    }
}

impl RateLimit {
    /// The effective burst capacity, defaulting to the sustained rate.
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        match &self.burst {
            Some(b) => b.capacity,
            None => self.sustained.rate,
        }
    }

    /// Tokens per second, expressed as `f64` for the refill clock.
    #[must_use]
    pub fn refill_per_sec(&self) -> f64 {
        let window = self.sustained.window.duration().as_secs_f64();
        f64::from(u32::try_from(self.sustained.rate).unwrap_or(u32::MAX)) / window
    }
}

/// CORS configuration (`docs/ADR/0004-cors.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct Cors {
    /// Sharing mode.
    pub sharing: SharingMode,
    /// CORS is off unless explicitly enabled.
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed methods.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the safelisted set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials. Requires specific origins, not `*`.
    pub allow_credentials: bool,
}

impl Default for Cors {
    fn default() -> Self {
        Self {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: Vec::new(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }
}

impl Cors {
    /// Reject `allow_credentials: true` together with a wildcard origin.
    ///
    /// # Errors
    /// Returns a message when the combination is present.
    pub fn validate(&self) -> Result<(), String> {
        if self.allow_credentials && self.allowed_origins.iter().any(|o| o == "*") {
            return Err(
                "allow_credentials cannot be combined with the wildcard origin '*'".to_owned(),
            );
        }
        Ok(())
    }

    /// `true` when `origin` is allowed.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|o| o == "*" || o == origin)
    }

    /// `true` when `method` is allowed.
    #[must_use]
    pub fn allows_method(&self, method: &str) -> bool {
        self.allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method))
    }
}

/// Authentication configuration on an upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier. The schema field is `type`; DESIGN.md
    /// refers to the same field as `auth.plugin_type`, so both spellings
    /// are accepted on input.
    #[serde(
        rename = "type",
        alias = "plugin_type",
        skip_serializing_if = "Option::is_none"
    )]
    pub auth_type: Option<String>,
    /// Sharing mode.
    #[serde(skip_serializing_if = "SharingMode::is_default")]
    pub sharing: SharingMode,
    /// Plugin configuration.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// `server` block of an upstream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct ServerConfig {
    /// Endpoints. At least one.
    #[serde(default)]
    pub endpoints: Vec<Endpoint>,
}

/// An upstream: one external service, addressable by alias.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct Upstream {
    /// System-generated UUID.
    pub id: String,
    /// Tenant that owns this upstream.
    pub tenant_id: String,
    /// Routing key in `/proxy/{alias}/…`.
    #[serde(default)]
    pub alias: String,
    /// `true` unless disabled.
    pub enabled: bool,
    /// Tags, unioned across the tenant hierarchy.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool.
    #[serde(default)]
    pub server: ServerConfig,
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`.
    #[serde(default)]
    pub protocol: String,
    /// Auth plugin configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginSet,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: String::new(),
            alias: String::new(),
            enabled: true,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: protocol::HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: PluginSet::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

/// HTTP match rules on a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct HttpMatch {
    /// Allowed methods, at least one.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path pattern the proxy suffix is appended to.
    #[serde(default)]
    pub path: String,
    /// Query parameters that may pass through. Empty allows none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Whether a path suffix is accepted.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: vec!["GET".to_owned()],
            path: String::new(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// Path-suffix handling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(utoipa::ToSchema)]
pub enum PathSuffixMode {
    /// Append the suffix to `path`.
    #[default]
    Append,
    /// Reject any request carrying a suffix.
    Disabled,
}

/// gRPC match rules (catalogued; no routable data path).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    #[serde(default)]
    pub service: String,
    /// RPC method name.
    #[serde(default)]
    pub method: String,
}

/// Protocol-scoped match rules. The schema nests these under `match` as
/// `{ "http": … }` or `{ "grpc": … }`, exactly one of which is required.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct RouteMatch {
    /// HTTP matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC matching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// A route binding an inbound match rule to an upstream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct Route {
    /// System-generated UUID.
    pub id: String,
    /// Tenant that owns this route.
    pub tenant_id: String,
    /// Upstream this route targets. Immutable after creation.
    pub upstream_id: String,
    /// Ordering key for otherwise equal prefixes.
    #[serde(default)]
    pub priority: i64,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Match rule.
    #[serde(rename = "match")]
    pub matcher: RouteMatch,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginSet,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,
    /// `true` unless disabled; disabled routes are excluded from matching.
    pub enabled: bool,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: String::new(),
            upstream_id: String::new(),
            priority: 0,
            tags: Vec::new(),
            matcher: RouteMatch::default(),
            plugins: PluginSet::default(),
            rate_limit: None,
            cors: None,
            enabled: true,
        }
    }
}

/// A stored custom plugin definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(utoipa::ToSchema)]
pub struct Plugin {
    /// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`.
    pub id: String,
    /// Tenant that owns this plugin.
    pub tenant_id: String,
    /// `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Human-readable name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Starlark source, when the plugin is scripted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Plugin configuration.
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: String::new(),
            plugin_type: "guard".to_owned(),
            name: None,
            source: None,
            config: BTreeMap::new(),
        }
    }
}

/// Strip a `cred://` prefix, tolerating a bare reference.
#[must_use]
pub fn strip_cred_prefix(reference: &str) -> &str {
    reference.strip_prefix("cred://").unwrap_or(reference)
}

/// Normalize an alias: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Validate an RFC 1123 hostname (`docs/DESIGN.md` §3.2).
///
/// # Errors
/// Returns a message naming the violated rule.
pub fn validate_hostname(host: &str) -> Result<(), String> {
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return Err("hostname must not be empty".to_owned());
    }
    if host.len() > 253 {
        return Err(format!("hostname exceeds 253 characters ({})", host.len()));
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(format!("hostname '{host}' contains an empty label"));
        }
        if label.len() > 63 {
            return Err(format!("hostname label '{label}' exceeds 63 characters"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "label '{label}' may not start or end with a hyphen"
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!(
                "label '{label}' contains characters outside RFC 1123"
            ));
        }
    }
    Ok(())
}

/// GTS id for a resource instance of `type_name`.
#[must_use]
pub fn gts_resource_id(type_name: &str, uuid: &str) -> String {
    format!("gts.cf.core.oagw.{type_name}.v1~{uuid}")
}

/// Extract the bare UUID from either a bare UUID or a GTS identifier.
#[must_use]
pub fn uuid_from_resource_id(value: &str) -> Option<String> {
    match value.find('~') {
        Some(idx) => {
            let tail = &value[idx + 1..];
            (!tail.is_empty()).then(|| tail.to_owned())
        }
        None => Some(value.to_owned()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn ip_detection_covers_v4_and_v6() {
        assert!(is_ip_literal("10.0.1.1"));
        assert!(is_ip_literal("127.0.0.1"));
        assert!(is_ip_literal("::1"));
        assert!(is_ip_literal("[::1]"));
        assert!(!is_ip_literal("api.openai.com"));
        assert!(!is_ip_literal("api.openai.com:8443"));
        assert!(!is_ip_literal("not-an-ip"));
    }

    #[test]
    fn standard_ports_are_per_scheme() {
        assert!(is_standard_port("https", 443));
        assert!(is_standard_port("wss", 443));
        assert!(is_standard_port("wt", 443));
        assert!(is_standard_port("grpc", 443));
        assert!(is_standard_port("http", 80));
        assert!(!is_standard_port("https", 8443));
        assert!(!is_standard_port("http", 8080));
    }

    #[test]
    fn authority_uses_the_port_when_non_standard() {
        let e = |scheme: &str, host: &str, port: u16| Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        };
        assert_eq!(
            e("https", "api.openai.com", 443).authority(),
            "api.openai.com"
        );
        assert_eq!(
            e("https", "Api.OpenAI.COM", 8443).authority(),
            "api.openai.com:8443"
        );
        assert!(e("https", "api.openai.com", 443).is_secure());
        assert!(!e("http", "api.openai.com", 80).is_secure());
    }

    #[test]
    fn hostname_rules() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("a-b.example.co.uk.").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("a..b").is_err());
        assert!(validate_hostname("-bad").is_err());
        assert!(validate_hostname("bad-").is_err());
        assert!(validate_hostname("has space.com").is_err());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(64))).is_err());
        // DESIGN.md: 253 characters in total.
        let many = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61),
        ]
        .join(".");
        assert_eq!(many.len(), 253);
        assert!(validate_hostname(&many).is_ok());
        assert!(validate_hostname(&format!("{many}x")).is_err());
    }

    #[test]
    fn aliases_normalize_to_lowercase_without_trailing_dot() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize_alias("  api.example.com  "), "api.example.com");
    }

    #[test]
    fn resource_ids_round_trip() {
        let gts = gts_resource_id("upstream", "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            gts,
            "gts.cf.core.oagw.upstream.v1~550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(
            uuid_from_resource_id(&gts).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            uuid_from_resource_id("550e8400-e29b-41d4-a716-446655440000").as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(uuid_from_resource_id("gts.x~"), None);
    }

    #[test]
    fn credential_prefix_is_optional() {
        assert_eq!(strip_cred_prefix("cred://openai-key"), "openai-key");
        assert_eq!(strip_cred_prefix("openai-key"), "openai-key");
    }

    #[test]
    fn cors_rejects_credentials_with_wildcard() {
        let mut cors = Cors {
            enabled: true,
            allow_credentials: true,
            allowed_origins: vec!["*".to_owned()],
            ..Cors::default()
        };
        assert!(cors.validate().is_err());
        cors.allowed_origins = vec!["https://app.example.com".to_owned()];
        assert!(cors.validate().is_ok());
        assert!(cors.allows_method("post"));
        assert!(!cors.allows_origin("https://other.example.com"));
    }

    #[test]
    fn plugin_bindings_accept_both_documented_shapes() {
        let bare: PluginBinding = serde_json::from_value(serde_json::json!(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        ))
        .expect("bare ref");
        assert_eq!(
            bare.id(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert!(bare.config().is_none());

        let bound: PluginBinding = serde_json::from_value(serde_json::json!({
            "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "config": { "required_request_headers": "x-correlation-id,accept" }
        }))
        .expect("bound ref");
        assert_eq!(
            bound.config_str("required_request_headers").as_deref(),
            Some("x-correlation-id,accept")
        );
    }

    #[test]
    fn rate_limit_defaults_burst_to_sustained_rate() {
        let rl = RateLimit {
            sustained: SustainedRate {
                rate: 100,
                window: RateWindow::Second,
            },
            ..RateLimit::default()
        };
        assert_eq!(rl.capacity(), 100);
        assert!((rl.refill_per_sec() - 100.0).abs() < f64::EPSILON);
        assert_eq!(
            RateLimit {
                sustained: SustainedRate {
                    rate: 10,
                    window: RateWindow::Minute
                },
                ..RateLimit::default()
            }
            .refill_per_sec(),
            10.0 / 60.0
        );
    }

    #[test]
    fn upstream_defaults_match_the_schema() {
        let u: Upstream = serde_json::from_value(serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }))
        .expect("parses");
        assert!(u.enabled);
        assert!(u.alias.is_empty());
        assert_eq!(u.server.endpoints[0].port, 443);
        assert_eq!(u.plugins.sharing, SharingMode::Private);
        assert!(u.plugins.items.is_empty());
        assert!(u.auth.is_none());
    }

    #[test]
    fn route_match_is_nested_under_a_named_key() {
        let r: Route = serde_json::from_value(serde_json::json!({
            "upstream_id": "550e8400-e29b-41d4-a716-446655440000",
            "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/chat" } }
        }))
        .expect("parses");
        let http = r.matcher.http.as_ref().expect("http match");
        assert_eq!(http.methods.len(), 2);
        assert_eq!(http.path, "/v1/chat");
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
        assert!(r.matcher.grpc.is_none());
        assert!(r.enabled);
        assert_eq!(r.priority, 0);
    }

    #[test]
    fn auth_accepts_type_and_plugin_type_keys() {
        let by_type: AuthConfig = serde_json::from_value(serde_json::json!({
            "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            "config": { "key_ref": "cred://k" }
        }))
        .expect("parses");
        assert_eq!(
            by_type.auth_type.as_deref(),
            Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
        );
        let by_alias: AuthConfig = serde_json::from_value(serde_json::json!({
            "plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
        }))
        .expect("parses");
        assert_eq!(
            by_alias.auth_type.as_deref(),
            Some("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1")
        );
    }

    #[test]
    fn upstream_rejects_unknown_keys() {
        let body = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "h" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "nonsense": 1
        });
        assert!(serde_json::from_value::<Upstream>(body).is_err());
    }
}
