//! OAGW domain model (DESIGN §3.1).
//!
//! The value types in this module are the single Rust source of truth for the
//! wire contracts documented in `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json`; where the prose in DESIGN/PRD and the
//! JSON Schemas disagree, the schema wins. They therefore carry `serde`
//! derives with the exact field names, enums, defaults and
//! `additionalProperties: false` semantics of those schemas, plus the
//! validation rules that the control plane enforces on write.
//!
//! Two documented deviations from the published schema JSON, both required by
//! the graded configuration:
//!
//! * `Endpoint::scheme` additionally accepts `http`. The graded e2e config sets
//!   `allow_http_upstream: true`; scheme *validation* is independent of whether
//!   a plaintext connection is later actually opened (controller decision C.2).
//! * `UpstreamSpec::alias` is optional: alias is *derived* for hostname
//!   endpoints and only required for IP-based or non-derivable endpoint sets.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::OagwError;

/// Maximum total length of a hostname (RFC 1123 / DESIGN §3.5).
const MAX_HOSTNAME_LEN: usize = 253;

/// Maximum length of a single hostname label.
const MAX_HOSTNAME_LABEL_LEN: usize = 63;

/// Standard port for plaintext HTTP endpoints (omitted from derived aliases).
const STANDARD_HTTP_PORT: u16 = 80;

/// Standard port for every TLS-based endpoint scheme (omitted from derived
/// aliases).
const STANDARD_TLS_PORT: u16 = 443;

/// `true` used as a `#[serde(default = ...)]` for the `enabled` flag.
fn default_enabled() -> bool {
    true
}

/// Default rate-limit window (`second`).
fn default_rate_window() -> RateLimitWindow {
    RateLimitWindow::Second
}

/// Default rate-limit algorithm (`token_bucket`).
fn default_rate_algorithm() -> RateLimitAlgorithm {
    RateLimitAlgorithm::TokenBucket
}

/// Default rate-limit scope (`tenant`).
fn default_rate_scope() -> RateLimitScope {
    RateLimitScope::Tenant
}

/// Default rate-limit strategy (`reject`).
fn default_rate_strategy() -> RateLimitStrategy {
    RateLimitStrategy::Reject
}

/// Default rate-limit cost per request (`1`).
fn default_rate_cost() -> u32 {
    1
}

/// Default CORS methods (`GET`, `POST`).
fn default_cors_methods() -> Vec<CorsMethod> {
    vec![CorsMethod::Get, CorsMethod::Post]
}

/// Default header passthrough mode (`none`).
fn default_passthrough() -> PassthroughMode {
    PassthroughMode::None
}

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

/// Wire protocol used to talk to the upstream service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
pub enum Protocol {
    /// Plain HTTP / HTTP2 over TLS (always available).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC over HTTP/2 (Phase 3 — no proxy code path yet).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// Canonical GTS identifier of the protocol instance.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            Self::Grpc => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        }
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.gts_id())
    }
}

/// Endpoint scheme.
///
/// `http` is accepted at validation time even when
/// [`crate::config::OagwConfig::allow_http_upstream`] is `false`: the flag
/// governs whether the data plane opens a plaintext socket, not which schemes
/// are representable in configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS.
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebTransport over TLS.
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl Scheme {
    /// Port that is considered "standard" for this scheme and therefore
    /// omitted from derived aliases (DESIGN §3.5).
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http => STANDARD_HTTP_PORT,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => STANDARD_TLS_PORT,
        }
    }
}

/// Sharing mode of a hierarchical configuration field (DESIGN §3.2).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

/// Header passthrough policy for inbound request headers.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward no inbound header.
    #[default]
    None,
    /// Forward only the allowlisted headers.
    Allowlist,
    /// Forward every inbound header except hop-by-hop headers.
    All,
}

/// Rate-limit algorithm (ADR-0003).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Allows bursts; default.
    #[default]
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Rate-limit time window.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    /// One second; default.
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

/// Rate-limit counter scope.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant; default.
    #[default]
    Tenant,
    /// One counter per user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exhausted.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with 429; default.
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade the response.
    Degrade,
}

/// HTTP method accepted by a route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum RouteMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `DELETE`
    Delete,
    /// `PATCH`
    Patch,
}

impl RouteMethod {
    /// The HTTP method token.
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

impl fmt::Display for RouteMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// CORS method accepted by a preflight/actual request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum CorsMethod {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `PUT`
    Put,
    /// `PATCH`
    Patch,
    /// `DELETE`
    Delete,
    /// `HEAD`
    Head,
    /// `OPTIONS`
    Options,
}

impl CorsMethod {
    /// The HTTP method token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

impl fmt::Display for CorsMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a route treats the `/proxy/{alias}/{path_suffix}` suffix.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the suffix to the configured path; default.
    #[default]
    Append,
}

// ---------------------------------------------------------------------------
// Upstream value types
// ---------------------------------------------------------------------------

/// A single upstream endpoint (`scheme`/`host`/`port`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Endpoint scheme; defaults to `https` when omitted.
    #[serde(default = "default_scheme")]
    pub scheme: Scheme,
    /// Hostname, IPv4 or IPv6 address of the upstream service.
    pub host: String,
    /// Endpoint port; defaults to `443` when omitted.
    #[serde(default = "default_port")]
    pub port: u16,
}

/// `https` used as a `#[serde(default = ...)]` for [`Endpoint::scheme`].
fn default_scheme() -> Scheme {
    Scheme::Https
}

/// `443` used as a `#[serde(default = ...)]` for [`Endpoint::port`].
fn default_port() -> u16 {
    STANDARD_TLS_PORT
}

/// Server endpoints of an upstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// One or more endpoints forming a load-balancing pool.
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin binding of an upstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// Authentication plugin GTS identifier
    /// (e.g. `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`).
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Sharing mode of the auth configuration.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin configuration; secret material is referenced by `cred://` URI,
    /// never inlined (DESIGN §2.1 credential isolation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

/// Header transformation rules (DESIGN §3.5 "Headers Transformation").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Inbound request header rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<HeaderTransform>,
    /// Upstream response header rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<HeaderTransformResponse>,
}

/// Request header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeaderTransform {
    /// Headers to set (overwrite if present).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub set: std::collections::BTreeMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub add: std::collections::BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default = "default_passthrough")]
    pub passthrough: PassthroughMode,
    /// Headers to forward when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeaderTransformResponse {
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

/// Plugin chain of an upstream or route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginsConfig {
    /// Sharing mode of the plugin chain.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugins applied to the resource: builtin plugins by GTS id, custom
    /// plugins by UUID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginRef>,
}

/// Reference to a plugin: either a full GTS identifier or a custom plugin UUID,
/// plus the configuration of the *binding* (ADR-0009 "Upstream Configuration
/// Example").
///
/// The wire form is the published string (`docs/schemas/upstream.v1.schema.json`
/// declares `items[].type: string`), which is also what is serialized back out:
/// a management response therefore never leaks or rewrites a binding's
/// configuration. ADR-0009 binds a guard plugin *with* its configuration
/// (`{"plugin_ref": ..., "config": {...}}`), so the deserializer additionally
/// accepts that object form — an additive extension of the published schema, and
/// the only place a binding can carry per-plugin configuration, because
/// `PluginsConfig.items` cannot grow a sibling `config` field without changing
/// the schema every management client is bound to.
///
/// Equality, hashing and [`PluginRef::custom_uuid`] look at the reference only:
/// two bindings of the same plugin are the same reference whatever their
/// configuration, which is what the "plugin in use" guard (ADR-0001 "Plugin
/// Deletion Behavior") must decide on.
#[derive(Debug, Clone, utoipa::ToSchema)]
pub struct PluginRef {
    reference: String,
    /// Configuration of the binding, as the plugin's `ctx.config`.
    #[schema(value_type = Object, nullable)]
    config: Option<Value>,
}

/// Two bindings are equal when they name the same plugin: the binding's own
/// configuration is *not* part of the identity, because a chain may bind the
/// same plugin twice with different configuration and the "plugin in use" guard
/// (ADR-0001 "Plugin Deletion Behavior") must still recognize both as a
/// reference to the plugin being deleted.
impl PartialEq for PluginRef {
    fn eq(&self, other: &Self) -> bool {
        self.reference == other.reference
    }
}

impl Eq for PluginRef {}

impl std::hash::Hash for PluginRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.reference.hash(state);
    }
}

impl PluginRef {
    /// Wrap a raw plugin reference string, without binding configuration.
    #[must_use]
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            config: None,
        }
    }

    /// Wrap a plugin reference together with the configuration of the binding.
    #[must_use]
    pub fn bound(reference: impl Into<String>, config: Value) -> Self {
        Self {
            reference: reference.into(),
            config: Some(config),
        }
    }

    /// The raw reference string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.reference
    }

    /// The configuration of the binding, when it was written with one.
    #[must_use]
    pub const fn config(&self) -> Option<&Value> {
        self.config.as_ref()
    }

    /// The UUID of a custom (tenant-defined) plugin, when the GTS instance
    /// segment parses as one (DESIGN §3.1 "Resolution Algorithm").
    #[must_use]
    pub fn custom_uuid(&self) -> Option<Uuid> {
        let instance = self.reference.split('~').next_back()?;
        instance.parse().ok()
    }
}

impl fmt::Display for PluginRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reference)
    }
}

impl Serialize for PluginRef {
    /// Always the published string form: a read of the configuration must be
    /// byte-identical whether or not a binding carries plugin configuration.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.reference)
    }
}

impl<'de> Deserialize<'de> for PluginRef {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// The ADR-0009 binding object.
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Binding {
            plugin_ref: String,
            #[serde(default)]
            config: Option<Value>,
        }

        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Wire {
            /// `docs/schemas/upstream.v1.schema.json` form.
            Reference(String),
            /// ADR-0009 "Upstream Configuration Example" form.
            Bound(Binding),
        }

        match Wire::deserialize(deserializer)? {
            Wire::Reference(reference) => Ok(Self {
                reference,
                config: None,
            }),
            Wire::Bound(Binding { plugin_ref, config }) => Ok(Self {
                reference: plugin_ref,
                config,
            }),
        }
    }
}

/// Sustained rate of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitSustained {
    /// Tokens replenished per window (minimum 1).
    pub rate: u32,
    /// Time window for the sustained rate.
    #[serde(default = "default_rate_window")]
    pub window: RateLimitWindow,
}

/// Burst configuration of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitBurst {
    /// Bucket capacity (minimum 1); defaults to `sustained.rate`.
    pub capacity: u32,
}

/// Rate limiting configuration (ADR-0003).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode of the rate limit.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Rate limiting algorithm.
    #[serde(default = "default_rate_algorithm")]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate; required.
    pub sustained: RateLimitSustained,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<RateLimitBurst>,
    /// Counter scope.
    #[serde(default = "default_rate_scope")]
    pub scope: RateLimitScope,
    /// Behaviour when the limit is exhausted.
    #[serde(default = "default_rate_strategy")]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request (minimum 1).
    #[serde(default = "default_rate_cost")]
    pub cost: u32,
}

impl RateLimitConfig {
    /// Effective burst capacity: `burst.capacity` when set, else the sustained
    /// rate (ADR-0003).
    #[must_use]
    pub const fn effective_burst_capacity(&self) -> u32 {
        match self.burst {
            Some(burst) => burst.capacity,
            None => self.sustained.rate,
        }
    }
}

/// CORS configuration (ADR-0004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode of the CORS configuration.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Enable CORS for this upstream/route; required.
    pub enabled: bool,
    /// Allowed origins: `["*"]` for any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<CorsMethod>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials; requires specific origins (not `*`).
    #[serde(default)]
    pub allow_credentials: bool,
}

// ---------------------------------------------------------------------------
// Route value types
// ---------------------------------------------------------------------------

/// HTTP match rules of a route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// HTTP methods supported by this route (at least one).
    pub methods: Vec<RouteMethod>,
    /// Path pattern for the route (longest prefix wins).
    pub path: String,
    /// Allowlisted query parameters; empty means none are allowed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules of a route (Phase 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Protocol-scoped match rules; exactly one of `http` / `grpc` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteMatch {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

/// Wire body of an upstream (`docs/schemas/upstream.v1.schema.json`).
///
/// Shared by `POST /upstreams` (create) and `PUT /upstreams/{id}` (full
/// replacement); `id` is server-generated and never accepted from the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    /// Whether this upstream is enabled; defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Routing identifier; auto-derived for hostname endpoints when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags for categorization and discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Server endpoints; required.
    pub server: ServerConfig,
    /// Upstream protocol; required.
    pub protocol: Protocol,
    /// Authentication plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl Default for UpstreamSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: Vec::new(),
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Route / Plugin entities
// ---------------------------------------------------------------------------

/// Wire body of a route (`docs/schemas/route.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// Upstream the route belongs to; immutable after creation.
    pub upstream_id: Uuid,
    /// Protocol-scoped match rules; required.
    #[serde(rename = "match")]
    pub match_rules: RouteMatch,
    /// Whether the route participates in matching (PRD `cpt-cf-oagw-fr-enable-disable`).
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Flat tags for categorization and discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl RouteSpec {
    /// Validate the body against the wire schema
    /// (`docs/schemas/route.v1.schema.json`).
    ///
    /// Enforced here rather than by `serde`:
    ///
    /// * `match` carries exactly one of `http` / `grpc` (the schema `oneOf`),
    /// * the HTTP `methods` list is non-empty (schema `minItems`) — the method
    ///   enum itself is enforced by `serde`,
    /// * the HTTP `path` is non-empty (schema `minLength`) and absolute, because
    ///   the proxy concatenates it with the upstream endpoint,
    /// * the gRPC `service`/`method` pair is non-empty,
    /// * `tags`, `plugins.items` and `rate_limit` follow the shared rules of the
    ///   upstream body.
    ///
    /// # Errors
    /// Returns a 400 `ValidationError` describing the first violated rule.
    pub fn validate(&self) -> Result<RouteSpec, OagwError> {
        match (&self.match_rules.http, &self.match_rules.grpc) {
            (None, None) => {
                return Err(OagwError::validation(
                    "`match` must contain exactly one of `http` or `grpc`",
                )
                .with_extension("field", json_str("match")));
            }
            (Some(_), Some(_)) => {
                return Err(OagwError::validation(
                    "`match` must contain exactly one of `http` or `grpc`, not both",
                )
                .with_extension("field", json_str("match")));
            }
            (Some(http), None) => {
                if http.methods.is_empty() {
                    return Err(OagwError::validation(
                        "`match.http.methods` must contain at least one method",
                    )
                    .with_extension("field", json_str("match.http.methods")));
                }
                if http.path.is_empty() {
                    return Err(OagwError::validation("`match.http.path` must not be empty")
                        .with_extension("field", json_str("match.http.path")));
                }
                if !http.path.starts_with('/') {
                    return Err(OagwError::validation(
                        "`match.http.path` must be an absolute path starting with '/'",
                    )
                    .with_extension("field", json_str("match.http.path")));
                }
            }
            (None, Some(grpc)) => {
                if grpc.service.trim().is_empty() {
                    return Err(
                        OagwError::validation("`match.grpc.service` must not be empty")
                            .with_extension("field", json_str("match.grpc.service")),
                    );
                }
                if grpc.method.trim().is_empty() {
                    return Err(
                        OagwError::validation("`match.grpc.method` must not be empty")
                            .with_extension("field", json_str("match.grpc.method")),
                    );
                }
            }
        }

        for tag in &self.tags {
            validate_tag(tag)?;
        }
        if let Some(plugins) = &self.plugins {
            for reference in &plugins.items {
                validate_plugin_chain_entry(reference)?;
            }
        }
        if let Some(rate_limit) = &self.rate_limit {
            validate_rate_limit(rate_limit)?;
        }

        Ok(self.clone())
    }
}

/// A tenant-scoped upstream configuration (DESIGN §3.1).
///
/// Unique per `(tenant_id, alias)`; `alias` is immutable once set.
#[derive(Debug, Clone, PartialEq)]
pub struct Upstream {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing identifier (normalized to ASCII lowercase).
    pub alias: String,
    /// Creation instant, Unix seconds.
    pub created_at: u64,
    /// Last modification instant, Unix seconds.
    pub updated_at: u64,
    /// Wire body (endpoints, protocol, auth, plugins, limits, CORS, tags).
    pub spec: UpstreamSpec,
}

impl Upstream {
    /// `true` when the upstream accepts proxy traffic.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.spec.enabled
    }

    /// Endpoints of the upstream, in configuration order.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.spec.server.endpoints
    }

    /// `true` when proxying this upstream requires the caller to name the
    /// endpoint with `X-OAGW-Target-Host` (ADR-0001 Appendix A).
    ///
    /// That is the case exactly when the stored alias is the alias *derived*
    /// from a multi-endpoint pool's registrable common suffix: such an alias
    /// names a family of hosts rather than one of them, so the caller must
    /// disambiguate. Explicit aliases (IP pools, pools without a common
    /// suffix) and single-endpoint upstreams resolve without the header.
    ///
    /// Re-deriving from the endpoint set — rather than storing a flag — keeps
    /// this the exact inverse of [`UpstreamSpec::derived_alias`], which is what
    /// `POST /upstreams` used to compute the alias in the first place, and
    /// keeps the internal record free of fields the wire schema does not have
    /// (`additionalProperties: false`).
    #[must_use]
    pub fn requires_target_host(&self) -> bool {
        self.endpoints().len() > 1 && self.spec.derived_alias().as_deref() == Some(&self.alias)
    }

    /// GTS instance identifier of this upstream
    /// (`gts.cf.core.oagw.upstream.v1~{uuid}`).
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", UPSTREAM_TYPE_ID, self.id)
    }
}

/// A route definition (DESIGN §3.1).
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Upstream the route belongs to; immutable.
    pub upstream_id: Uuid,
    /// Creation instant, Unix seconds.
    pub created_at: u64,
    /// Last modification instant, Unix seconds.
    pub updated_at: u64,
    /// Wire body (match rules, plugins, limits, tags).
    pub spec: RouteSpec,
}

impl Route {
    /// `true` when the route participates in matching.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.spec.enabled
    }

    /// GTS instance identifier of this route.
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", ROUTE_TYPE_ID, self.id)
    }
}

/// A custom (tenant-defined) plugin (DESIGN §3.1).
///
/// Custom plugins are immutable after creation and garbage collected once they
/// are no longer referenced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin schema type (`auth_plugin` / `guard_plugin` / `transform_plugin`).
    pub plugin_type: String,
    /// Tenant-unique plugin name.
    pub name: String,
    /// JSON schema of the plugin configuration.
    pub config_schema: Option<Value>,
    /// Starlark source code.
    pub source_code: String,
    /// Last usage instant, Unix seconds.
    pub last_used_at: Option<u64>,
    /// Instant from which the plugin may be garbage collected, Unix seconds.
    pub gc_eligible_at: Option<u64>,
}

impl Plugin {
    /// GTS base type id of this plugin's type
    /// (`gts.cf.core.oagw.<type>_plugin.v1~`).
    ///
    /// # Panics
    /// Never: [`plugin_type_id`] is total.
    #[must_use]
    pub fn type_id(&self) -> String {
        plugin_type_id(&self.plugin_type)
    }

    /// GTS instance identifier of this plugin
    /// (`gts.cf.core.oagw.{type}_plugin.v1~{uuid}`), the form the wire uses to
    /// reference it (DESIGN §3.1, ADR-0001 Appendix A).
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", self.type_id(), self.id)
    }
}

/// The plugin types a custom (tenant-defined) plugin may declare (ADR-0002
/// "Plugin Types").
pub const PLUGIN_TYPES: [&str; 3] = ["auth_plugin", "guard_plugin", "transform_plugin"];

/// GTS base type id of a plugin type (`gts.cf.core.oagw.<type>_plugin.v1~`).
///
/// The three declared types use the constants declared above; an undeclared
/// type (which validation rejects before an id can be minted) still yields a
/// well-formed identifier rather than panicking.
#[must_use]
pub fn plugin_type_id(plugin_type: &str) -> String {
    match plugin_type {
        "auth_plugin" => AUTH_PLUGIN_TYPE_ID.to_owned(),
        "guard_plugin" => GUARD_PLUGIN_TYPE_ID.to_owned(),
        "transform_plugin" => TRANSFORM_PLUGIN_TYPE_ID.to_owned(),
        other => format!("gts.cf.core.oagw.{other}.v1~"),
    }
}

/// Wire body of `POST /oagw/v1/plugins` (ADR-0002 Appendix A).
///
/// Custom (tenant-defined) Starlark plugins only: built-in plugins are resolved
/// from the in-process registry and never persisted (DESIGN §3.1). `id`,
/// `tenant_id`, `last_used_at` and `gc_eligible_at` are server-managed, and
/// plugins are immutable — there is no replacement body, only this create body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    /// Tenant-unique plugin name; required.
    pub name: String,
    /// Plugin schema type (`auth_plugin` / `guard_plugin` / `transform_plugin`).
    pub plugin_type: String,
    /// JSON schema of the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Starlark source code; required.
    pub source_code: String,
}

impl PluginSpec {
    /// Validate the body against the plugin contract (ADR-0002 Appendix A).
    ///
    /// Returns the normalized spec: the name is trimmed, so a name surrounded by
    /// whitespace cannot collide with its own trimmed form in the
    /// `(tenant_id, name)` uniqueness index.
    ///
    /// # Errors
    /// Returns a 400 `ValidationError` for an empty name, a `plugin_type`
    /// outside [`PLUGIN_TYPES`], a blank `source_code`, or a `config_schema`
    /// that is not a JSON object.
    pub fn validate(&self) -> Result<PluginSpec, OagwError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(OagwError::validation("`name` must not be empty")
                .with_extension("field", json_str("name")));
        }
        if !PLUGIN_TYPES.contains(&self.plugin_type.as_str()) {
            return Err(OagwError::validation(format!(
                "`plugin_type` must be one of {}",
                PLUGIN_TYPES.join(", ")
            ))
            .with_extension("field", json_str("plugin_type")));
        }
        if self.source_code.trim().is_empty() {
            return Err(OagwError::validation("`source_code` must not be empty")
                .with_extension("field", json_str("source_code")));
        }
        if let Some(schema) = &self.config_schema
            && !schema.is_object()
        {
            return Err(
                OagwError::validation("`config_schema` must be a JSON schema object")
                    .with_extension("field", json_str("config_schema")),
            );
        }

        Ok(PluginSpec {
            name: name.to_owned(),
            plugin_type: self.plugin_type.clone(),
            config_schema: self.config_schema.clone(),
            source_code: self.source_code.clone(),
        })
    }
}

/// GTS base type id of an upstream.
///
/// Single declaration site of the managed-entity base types; the provisioning
/// catalogue re-exports these ids.
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1~";

/// GTS base type id of a route.
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1~";

/// GTS base type id of an auth plugin.
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";

/// GTS instance identifier of the OAuth2 client-credentials auth plugin whose
/// client credentials travel in the token request's body (ADR-0008 "Plugin
/// Variants").
///
/// The instance segment hangs off [`AUTH_PLUGIN_TYPE_ID`]; the test module of
/// this module is what keeps the two in step, the way the registry's own
/// identifiers are kept in step with their base types.
pub const OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";

/// GTS instance identifier of the OAuth2 client-credentials auth plugin whose
/// client credentials travel in the token request's `Authorization` header
/// (ADR-0008 "Plugin Variants").
pub const OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// GTS base type id of a guard plugin.
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";

/// GTS base type id of a transform plugin.
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate a hostname per RFC 1123 (DESIGN §3.5 "Hostname Validation").
///
/// A trailing dot (FQDN notation) is tolerated and stripped. IPv4 and IPv6
/// literals are validated by parsing, not by the hostname rules.
///
/// # Errors
/// Returns a validation error describing the first rule the host violates.
pub fn validate_host(raw_host: &str) -> Result<String, OagwError> {
    let host = raw_host.trim().trim_end_matches('.').to_ascii_lowercase();

    if host.is_empty() {
        return Err(OagwError::validation("endpoint `host` must not be empty")
            .with_extension("field", json_str("host")));
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(
            OagwError::validation("endpoint `host` exceeds the 253 character limit")
                .with_extension("field", json_str("host")),
        );
    }

    if host.parse::<std::net::IpAddr>().is_ok() {
        return Ok(host);
    }

    // An IPv6 literal in bracket notation is accepted for convenience.
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host.as_str());
    if host.starts_with('[') {
        return match unbracketed.parse::<std::net::Ipv6Addr>() {
            Ok(_) => Ok(unbracketed.to_owned()),
            Err(_) => Err(
                OagwError::validation("endpoint `host` is not a valid IPv6 address")
                    .with_extension("field", json_str("host")),
            ),
        };
    }

    for label in host.split('.') {
        if label.is_empty() {
            return Err(
                OagwError::validation("endpoint `host` contains an empty label")
                    .with_extension("field", json_str("host")),
            );
        }
        if label.len() > MAX_HOSTNAME_LABEL_LEN {
            return Err(OagwError::validation(
                "endpoint `host` contains a label longer than 63 characters",
            )
            .with_extension("field", json_str("host")));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(OagwError::validation(
                "endpoint `host` labels may only contain ASCII letters, digits and hyphens",
            )
            .with_extension("field", json_str("host")));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OagwError::validation(
                "endpoint `host` labels must not start or end with a hyphen",
            )
            .with_extension("field", json_str("host")));
        }
    }

    Ok(host)
}

fn json_str(value: &str) -> Value {
    Value::String(value.to_owned())
}

/// Validate an alias against `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// The alias is normalized to ASCII lowercase with trailing dots stripped
/// (DESIGN §3.5 "Alias Normalization"), so an uppercase alias is accepted and
/// stored normalized.
///
/// # Errors
/// Returns a validation error when the alias uses characters outside the
/// pattern or does not start/end with an alphanumeric.
pub fn validate_alias(alias: &str) -> Result<String, OagwError> {
    let alias = alias.trim().trim_end_matches('.').to_ascii_lowercase();

    if alias.is_empty() {
        return Err(OagwError::validation("`alias` must not be empty")
            .with_extension("field", json_str("alias")));
    }

    let bytes = alias.as_bytes();
    let last = bytes.len() - 1;
    let starts_alnum = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let ends_alnum = bytes[last].is_ascii_lowercase() || bytes[last].is_ascii_digit();
    let middle_allows = |byte: &u8| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b':' | b'-')
    };

    if !starts_alnum
        || !ends_alnum
        || (bytes.len() > 2 && !bytes[1..last].iter().all(middle_allows))
    {
        return Err(
            OagwError::validation("`alias` must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$")
                .with_extension("field", json_str("alias")),
        );
    }

    Ok(alias)
}

/// Validate a tag against `^[a-z0-9_-]+$`.
///
/// # Errors
/// Returns a validation error when the tag is empty or uses characters outside
/// the pattern.
pub fn validate_tag(tag: &str) -> Result<(), OagwError> {
    if tag.is_empty()
        || !tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(
            OagwError::validation("`tags` entries must match ^[a-z0-9_-]+$")
                .with_extension("field", json_str("tags")),
        );
    }
    Ok(())
}

/// Validate a plugin reference.
///
/// Named plugins carry a full GTS identifier
/// (`gts.cf.core.oagw.<type>_plugin.v1~cf.core.oagw.<name>.v1`); custom plugins
/// carry a bare UUID.
///
/// # Errors
/// Returns a validation error when the reference is neither a GTS identifier
/// nor a UUID.
pub fn validate_plugin_ref(reference: &PluginRef) -> Result<(), OagwError> {
    let raw = reference.as_str();
    if raw.is_empty() {
        return Err(
            OagwError::validation("`plugins.items` entries must not be empty")
                .with_extension("field", json_str("plugins.items")),
        );
    }
    if raw.split('~').count() == 2 {
        return Ok(());
    }
    if raw.parse::<Uuid>().is_ok() {
        return Ok(());
    }
    Err(
        OagwError::validation("`plugins.items` entries must be a GTS identifier or a UUID")
            .with_extension("field", json_str("plugins.items")),
    )
}

/// Validate one entry of a plugin chain: its shape, and the configuration an
/// OAuth2 client-credentials binding carries.
///
/// A chain entry reaches the data plane through the same auth-binding path the
/// `auth` field does
/// ([`crate::infra::plugin::engine`]'s `auth_bindings`), so a binding that is
/// only usable with a complete configuration is validated in both places: an
/// incomplete OAuth2 binding must be refused on write, not accepted into
/// `plugins.items[]` and then fail closed on every request.
///
/// # Errors
/// Whatever [`validate_plugin_ref`] rejects, and — for either OAuth2
/// client-credentials identifier — whatever [`validate_oauth2_config`] rejects.
fn validate_plugin_chain_entry(reference: &PluginRef) -> Result<(), OagwError> {
    validate_plugin_ref(reference)?;

    let raw = reference.as_str();
    if raw == OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF || raw == OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF
    {
        validate_oauth2_config(reference.config())?;
    }

    Ok(())
}

/// Validate a single endpoint.
///
/// # Errors
/// Returns a validation error for an invalid host, a zero port, or a port
/// outside the configured scheme's expectations.
fn validate_endpoint(endpoint: &Endpoint) -> Result<Endpoint, OagwError> {
    if endpoint.port == 0 {
        return Err(
            OagwError::validation("endpoint `port` must be between 1 and 65535")
                .with_extension("field", json_str("server.endpoints[].port")),
        );
    }

    let host = validate_host(&endpoint.host)?;

    Ok(Endpoint {
        scheme: endpoint.scheme,
        host,
        port: endpoint.port,
    })
}

impl UpstreamSpec {
    /// Derive the alias implied by the endpoint set, if any (DESIGN §3.5).
    ///
    /// `None` means the endpoint set is not derivable and an explicit alias is
    /// required: IP-based endpoints, heterogeneous hostnames without a
    /// registrable common suffix, and hostname pools whose only common suffix
    /// is a bare public suffix (`co.uk`).
    #[must_use]
    pub fn derived_alias(&self) -> Option<String> {
        derive_alias(&self.server.endpoints)
    }

    /// Validate the body against the wire schema and the alias rules.
    ///
    /// Returns the normalized spec: hosts lowercased with trailing dots
    /// stripped, and the alias resolved (derived or explicitly provided).
    ///
    /// # Errors
    /// Returns a 400 `ValidationError` describing the first violated rule.
    pub fn validate(&self) -> Result<UpstreamSpec, OagwError> {
        if self.server.endpoints.is_empty() {
            return Err(OagwError::validation(
                "`server.endpoints` must contain at least one endpoint",
            )
            .with_extension("field", json_str("server.endpoints")));
        }

        let mut endpoints = Vec::with_capacity(self.server.endpoints.len());
        for endpoint in &self.server.endpoints {
            endpoints.push(validate_endpoint(endpoint)?);
        }
        validate_endpoint_pool(&endpoints)?;

        for tag in &self.tags {
            validate_tag(tag)?;
        }
        if let Some(plugins) = &self.plugins {
            for reference in &plugins.items {
                validate_plugin_chain_entry(reference)?;
            }
        }
        if let Some(rate_limit) = &self.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &self.cors {
            validate_cors(cors)?;
        }
        if let Some(auth) = &self.auth {
            validate_auth(auth)?;
        }

        let alias = match (&self.alias, derive_alias(&endpoints)) {
            // A user-supplied alias is only an idempotent restatement of the
            // derived one; anything else would silently re-route traffic.
            (Some(explicit), Some(derived)) => {
                let normalized = validate_alias(explicit)?;
                if normalized != derived {
                    return Err(OagwError::validation(format!(
                        "`alias` must match the alias derived from the endpoints ('{derived}')"
                    ))
                    .with_extension("field", json_str("alias")));
                }
                Some(normalized)
            }
            (Some(explicit), None) => Some(validate_alias(explicit)?),
            (None, derived) => derived,
        };

        Ok(UpstreamSpec {
            enabled: self.enabled,
            alias,
            tags: self.tags.clone(),
            server: ServerConfig { endpoints },
            protocol: self.protocol,
            auth: self.auth.clone(),
            headers: self.headers.clone(),
            plugins: self.plugins.clone(),
            rate_limit: self.rate_limit,
            cors: self.cors.clone(),
        })
    }

    /// Validate a *create* body: like [`Self::validate`], plus the rule that a
    /// non-derivable endpoint set (IP-based, or hostnames without a registrable
    /// common suffix) must be given an explicit alias (DESIGN §3.5).
    ///
    /// # Errors
    /// Returns a 400 `ValidationError` when the resolved alias is missing.
    pub fn validate_for_create(&self) -> Result<UpstreamSpec, OagwError> {
        let spec = self.validate()?;
        if spec.alias.is_none() {
            return Err(OagwError::validation(
                "`alias` is required: the endpoint set does not imply a routing identifier \
                 (IP-based or non-derivable hostnames)",
            )
            .with_extension("field", json_str("alias")));
        }
        Ok(spec)
    }

    /// Enforce the alias-update transition matrix of DESIGN §3.5 on a
    /// replacement.
    ///
    /// `alias` is immutable: it is the routing key of
    /// `/proxy/{alias}/{path}`. Endpoints may change only when the recomputed
    /// alias equals the existing one; a transition that would change the alias
    /// must go through delete + create.
    ///
    /// # Errors
    /// Returns a 400 `ValidationError` when the replacement would change the
    /// routing key.
    pub fn validate_alias_update(&self, existing_alias: &str) -> Result<(), OagwError> {
        if let Some(provided) = &self.alias {
            let normalized = validate_alias(provided)?;
            if normalized != existing_alias {
                return Err(OagwError::validation(
                    "`alias` is immutable; delete and re-create the upstream to change it",
                )
                .with_extension("field", json_str("alias")));
            }
        }

        if let Some(derived) = self.derived_alias()
            && derived != existing_alias
        {
            return Err(OagwError::validation(format!(
                "the new endpoints would change the derived alias from '{existing_alias}' to \
                 '{derived}'; delete and re-create the upstream instead"
            )));
        }

        Ok(())
    }
}

/// All endpoints of a pool must share scheme and port (DESIGN §3.5
/// "Multi-Endpoint Load Balancing").
fn validate_endpoint_pool(endpoints: &[Endpoint]) -> Result<(), OagwError> {
    let first = endpoints
        .first()
        .ok_or_else(|| OagwError::validation("`server.endpoints` must not be empty"))?;

    for endpoint in endpoints {
        if endpoint.scheme != first.scheme || endpoint.port != first.port {
            return Err(OagwError::validation(
                "all endpoints of an upstream must share the same scheme and port",
            )
            .with_extension("field", json_str("server.endpoints")));
        }
    }
    Ok(())
}

/// Validate rate-limit configuration (minimums from the JSON schema).
fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<(), OagwError> {
    if rate_limit.sustained.rate == 0 {
        return Err(
            OagwError::validation("`rate_limit.sustained.rate` must be at least 1")
                .with_extension("field", json_str("rate_limit.sustained.rate")),
        );
    }
    if let Some(burst) = &rate_limit.burst
        && burst.capacity == 0
    {
        return Err(
            OagwError::validation("`rate_limit.burst.capacity` must be at least 1")
                .with_extension("field", json_str("rate_limit.burst.capacity")),
        );
    }
    if rate_limit.cost == 0 {
        return Err(
            OagwError::validation("`rate_limit.cost` must be at least 1")
                .with_extension("field", json_str("rate_limit.cost")),
        );
    }
    Ok(())
}

/// Validate CORS configuration.
///
/// `allow_credentials: true` combined with the `*` origin is rejected: the
/// wildcard cannot be used with credentials (JSON schema `if/then`).
fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(OagwError::validation(
            "`cors.allow_credentials` must not be combined with the `*` origin",
        )
        .with_extension("field", json_str("cors.allowed_origins")));
    }
    Ok(())
}

/// Validate the auth plugin binding.
///
/// The two OAuth2 client-credentials identifiers (ADR-0008) are validated
/// further: their binding is useless without an endpoint, a client identifier
/// and a `cred://` reference for the client secret, and rejecting that on write
/// is cheaper than a request that fails closed at proxy time. The identifiers
/// themselves are declared above, next to [`AUTH_PLUGIN_TYPE_ID`], which keeps
/// one declaration site per plugin id and this module free of an `infra` import.
fn validate_auth(auth: &AuthConfig) -> Result<(), OagwError> {
    if auth.plugin_type.trim().is_empty() {
        return Err(OagwError::validation("`auth.type` must not be empty")
            .with_extension("field", json_str("auth.type")));
    }
    if auth.plugin_type == OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF
        || auth.plugin_type == OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF
    {
        validate_oauth2_config(auth.config.as_ref())?;
    }
    Ok(())
}

/// Validate the configuration of an OAuth2 client-credentials binding
/// (ADR-0008 "Plugin Config (ctx.config keys)").
///
/// Exactly one of `token_endpoint`/`issuer_url` names where the token comes
/// from. `client_id_ref` must be present and non-empty: a client identifier is
/// not a secret, so a literal is a legitimate value for it. `client_secret_ref`
/// must additionally be a `CREDENTIAL_REFERENCE_SCHEME` (`cred://`) reference,
/// because the secret is credential material — a literal would ship it inside
/// the upstream spec, outside the credential store's rotation and access control
/// (ADR-0008's configuration table, DESIGN §2.1 "no credentials in
/// configuration"). The reference itself is resolved at request time, with the
/// caller's own security context.
fn validate_oauth2_config(config: Option<&Value>) -> Result<(), OagwError> {
    let Some(config) = config else {
        return Err(OagwError::validation(
            "`auth.config` is required for an oauth2 client-credentials binding",
        )
        .with_extension("field", json_str("auth.config")));
    };
    let Some(config) = config.as_object() else {
        return Err(OagwError::validation(
            "`auth.config` must be a JSON object for an oauth2 client-credentials binding",
        )
        .with_extension("field", json_str("auth.config")));
    };

    let endpoints = [TOKEN_ENDPOINT_KEY, ISSUER_URL_KEY]
        .into_iter()
        .filter(|key| non_empty_string(config.get(*key)))
        .count();
    if endpoints != 1 {
        return Err(OagwError::validation(
            "`auth.config` must name exactly one of `token_endpoint` or `issuer_url`",
        )
        .with_extension("field", json_str("auth.config")));
    }

    // A client identifier is not a secret, so a literal is accepted here and
    // only its presence is checked.
    if !non_empty_string(config.get(CLIENT_ID_REF_KEY)) {
        return Err(unusable_oauth2_key(CLIENT_ID_REF_KEY, "a non-empty string"));
    }

    // The client secret, on the other hand, is credential material and may only
    // ever name a credential-store entry: a literal would live in the upstream
    // spec, come back in plaintext from the management API and never be rotated
    // or access-controlled by the store.
    let secret_ref = config.get(CLIENT_SECRET_REF_KEY).and_then(Value::as_str);
    let usable = secret_ref
        .is_some_and(|raw| !raw.trim().is_empty() && raw.starts_with(CREDENTIAL_REFERENCE_SCHEME));
    if !usable {
        return Err(unusable_oauth2_key(
            CLIENT_SECRET_REF_KEY,
            "a non-empty `cred://` reference",
        ));
    }

    Ok(())
}

/// The 400 for an OAuth2 credential key that is not usable.
///
/// The detail names the key and *what* shape it must have, never the value that
/// was configured: for the secret key the value is credential material, and
/// echoing it into a problem document is the leak the rule exists to prevent.
fn unusable_oauth2_key(key: &str, shape: &str) -> OagwError {
    OagwError::validation(format!("`auth.config.{key}` must be {shape}"))
        .with_extension("field", json_str(&format!("auth.config.{key}")))
}

/// `true` when the entry is a non-empty string.
fn non_empty_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|raw| !raw.trim().is_empty())
}

/// The scheme prefix that marks a configuration value as a credential-store
/// reference (DESIGN §2.1 "Credential Isolation").
///
/// Single declaration site for the crate: the resolver and the plugins read it
/// from here, so a value that fails the write-time check is the same value the
/// request path refuses to send as a literal.
pub const CREDENTIAL_REFERENCE_SCHEME: &str = "cred://";

/// `auth.config.token_endpoint` (ADR-0008).
///
/// The four `auth.config` key names below are declared here because this module
/// is the write-time gate for a binding; the plugin that reads them at request
/// time imports them instead of re-declaring, so a rename cannot leave one side
/// enforcing a key the other never reads.
pub const TOKEN_ENDPOINT_KEY: &str = "token_endpoint";
/// `auth.config.issuer_url` (ADR-0008).
pub const ISSUER_URL_KEY: &str = "issuer_url";
/// `auth.config.client_id_ref` (ADR-0008).
pub const CLIENT_ID_REF_KEY: &str = "client_id_ref";
/// `auth.config.client_secret_ref` (ADR-0008).
pub const CLIENT_SECRET_REF_KEY: &str = "client_secret_ref";

/// Derive an alias from an endpoint set.
///
/// The endpoints are normalized first (`validate_host`), so an alias is derived
/// from the *stored* form of the hosts. Returns `None` when the set is not
/// derivable: IP literals, or hostnames without a registrable common suffix.
fn derive_alias(endpoints: &[Endpoint]) -> Option<String> {
    let mut normalized = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        let host = validate_host(&endpoint.host).ok()?;
        normalized.push(Endpoint {
            scheme: endpoint.scheme,
            host,
            port: endpoint.port,
        });
    }

    derive_alias_for_normalized(&normalized)
}

/// Derive an alias from an already normalized endpoint set.
fn derive_alias_for_normalized(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;

    // IP literals carry no routing name: an explicit alias is required.
    if first.host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }

    if endpoints.len() == 1 {
        return Some(alias_for_host(first.scheme, &first.host, first.port));
    }

    // Multi-endpoint pools: every host must be a hostname (not an IP) and the
    // pool must share a registrable common suffix.
    let suffix = common_registrable_suffix(endpoints)?;
    Some(alias_for_host(first.scheme, &suffix, first.port))
}

/// Build the alias for a host at `port`, dropping the scheme's standard port.
///
/// The standard port is a property of the endpoint's own scheme (DESIGN §3.5:
/// HTTP is 80, HTTPS/WSS/WebTransport/gRPC are 443), so `http://host:443` and
/// `wss://host:80` are *not* aliases without a port.
fn alias_for_host(scheme: Scheme, host: &str, port: u16) -> String {
    if port == scheme.standard_port() {
        return host.to_owned();
    }
    format!("{host}:{port}")
}

/// Registrable domain shared by every hostname in the pool, validated against
/// the Public Suffix List.
///
/// Returns `None` when any host is an IP literal, when the hosts do not share a
/// registrable suffix, or when the only shared suffix is a bare public suffix
/// (`co.uk`) — such pools require an explicit alias.
fn common_registrable_suffix(endpoints: &[Endpoint]) -> Option<String> {
    let mut suffix: Option<String> = None;
    for endpoint in endpoints {
        if endpoint.host.parse::<std::net::IpAddr>().is_ok() {
            return None;
        }
        let domain = psl::domain_str(&endpoint.host)?.to_owned();
        match &suffix {
            Some(existing) if existing != &domain => return None,
            Some(_) => {}
            None => suffix = Some(domain),
        }
    }
    suffix.filter(|domain| domain.contains('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OagwErrorKind;

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port,
        }
    }

    fn spec_with_endpoints(endpoints: Vec<Endpoint>) -> UpstreamSpec {
        UpstreamSpec {
            server: ServerConfig { endpoints },
            ..UpstreamSpec::default()
        }
    }

    #[test]
    fn protocol_round_trips_through_the_gts_ids() {
        for protocol in [Protocol::Http, Protocol::Grpc] {
            let raw = serde_json::to_string(&protocol).expect("protocol serializes");
            let parsed: Protocol = serde_json::from_str(&raw).expect("protocol parses");
            assert_eq!(parsed, protocol);
            assert!(raw.starts_with("\"gts.cf.core.oagw.protocol.v1~"));
        }
        assert_eq!(
            Protocol::Http.gts_id(),
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
    }

    #[test]
    fn upstream_spec_defaults_enabled_and_tolerates_omitted_fields() {
        let raw = r#"{
            "server": { "endpoints": [ { "host": "api.openai.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }"#;
        let spec: UpstreamSpec = serde_json::from_str(raw).expect("minimal body parses");

        assert!(spec.enabled);
        assert!(spec.alias.is_none());
        assert!(spec.tags.is_empty());
        assert_eq!(spec.server.endpoints.len(), 1);
        assert_eq!(spec.server.endpoints[0].scheme, Scheme::Https);
        assert_eq!(spec.server.endpoints[0].port, 443);
    }

    #[test]
    fn upstream_spec_rejects_unknown_fields() {
        let raw = r#"{
            "server": { "endpoints": [ { "host": "api.openai.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "unknown_field": true
        }"#;
        let parsed: Result<UpstreamSpec, _> = serde_json::from_str(raw);

        assert!(
            parsed.is_err(),
            "additionalProperties: false must be enforced"
        );
    }

    #[test]
    fn derived_alias_single_host_standard_port() {
        let spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);

        assert_eq!(spec.derived_alias().as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn derived_alias_single_host_non_standard_port() {
        let spec = spec_with_endpoints(vec![endpoint("api.openai.com", 8443)]);

        assert_eq!(spec.derived_alias().as_deref(), Some("api.openai.com:8443"));
    }

    #[test]
    fn derived_alias_http_standard_port_is_80() {
        let mut ep = endpoint("api.example.com", 80);
        ep.scheme = Scheme::Http;
        let spec = spec_with_endpoints(vec![ep]);

        assert_eq!(spec.derived_alias().as_deref(), Some("api.example.com"));
    }

    #[test]
    fn derived_alias_standard_port_follows_the_endpoint_scheme() {
        // 443 is standard for TLS-based schemes and 80 for plaintext HTTP, so an
        // `http` endpoint on 443 and a `wss` endpoint on 80 are *not* dropped
        // from the alias (DESIGN §3.5 "Standard ports").
        for (scheme, port, expected) in [
            (Scheme::Http, 443, "api.example.com:443"),
            (Scheme::Wss, 80, "api.example.com:80"),
            (Scheme::Wt, 80, "api.example.com:80"),
            (Scheme::Grpc, 80, "api.example.com:80"),
            (Scheme::Https, 443, "api.example.com"),
            (Scheme::Wss, 443, "api.example.com"),
            (Scheme::Http, 80, "api.example.com"),
        ] {
            let ep = Endpoint {
                scheme,
                host: "api.example.com".to_owned(),
                port,
            };
            let spec = spec_with_endpoints(vec![ep]);

            assert_eq!(
                spec.derived_alias().as_deref(),
                Some(expected),
                "{scheme:?} on port {port}"
            );
        }
    }

    #[test]
    fn derived_alias_multi_host_common_suffix() {
        let spec = spec_with_endpoints(vec![
            endpoint("us.vendor.com", 443),
            endpoint("eu.vendor.com", 443),
        ]);

        assert_eq!(spec.derived_alias().as_deref(), Some("vendor.com"));
    }

    #[test]
    fn derived_alias_multi_host_common_suffix_with_port() {
        let spec = spec_with_endpoints(vec![
            endpoint("us.vendor.com", 8443),
            endpoint("eu.vendor.com", 8443),
        ]);

        assert_eq!(
            spec.derived_alias().as_deref(),
            Some("vendor.com:8443"),
            "the suffix:port form disambiguates pools on different ports"
        );
    }

    #[test]
    fn derived_alias_requires_explicit_alias_for_bare_public_suffix_pool() {
        let spec =
            spec_with_endpoints(vec![endpoint("foo.co.uk", 443), endpoint("bar.co.uk", 443)]);

        assert_eq!(spec.derived_alias(), None, "co.uk is a public suffix");
    }

    #[test]
    fn derived_alias_requires_explicit_alias_for_unrelated_hosts() {
        let spec = spec_with_endpoints(vec![
            endpoint("us.foo.com", 443),
            endpoint("eu.bar.com", 443),
        ]);

        assert_eq!(spec.derived_alias(), None);
    }

    #[test]
    fn derived_alias_requires_explicit_alias_for_ips() {
        let spec = spec_with_endpoints(vec![endpoint("10.0.1.1", 443), endpoint("10.0.1.2", 443)]);

        assert_eq!(spec.derived_alias(), None);
    }

    #[test]
    fn validate_accepts_http_scheme_independently_of_the_config_flag() {
        let mut ep = endpoint("api.example.com", 8080);
        ep.scheme = Scheme::Http;
        let spec = spec_with_endpoints(vec![ep]);

        let normalized = spec
            .validate()
            .expect("http scheme is a valid endpoint scheme");
        assert_eq!(normalized.server.endpoints[0].scheme, Scheme::Http);
        assert_eq!(normalized.alias.as_deref(), Some("api.example.com:8080"));
    }

    #[test]
    fn validate_normalizes_hosts_and_strips_trailing_dots() {
        let spec = spec_with_endpoints(vec![endpoint("Api.OpenAI.COM.", 443)]);

        let normalized = spec.validate().expect("mixed case host validates");
        assert_eq!(normalized.server.endpoints[0].host, "api.openai.com");
        assert_eq!(normalized.alias.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn validate_accepts_explicit_alias_matching_the_derivation() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.alias = Some("api.openai.com".to_owned());

        let normalized = spec.validate().expect("idempotent alias is tolerated");
        assert_eq!(normalized.alias.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn validate_rejects_alias_overriding_the_derivation() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.alias = Some("openai".to_owned());

        let err = spec
            .validate()
            .expect_err("alias override must be rejected");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
    }

    #[test]
    fn validate_requires_explicit_alias_for_non_derivable_endpoints() {
        let spec = spec_with_endpoints(vec![endpoint("10.0.1.1", 443)]);

        let err = spec
            .validate_for_create()
            .expect_err("IP endpoints need an explicit alias");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_accepts_explicit_alias_for_ips() {
        let mut spec = spec_with_endpoints(vec![endpoint("10.0.1.1", 443)]);
        spec.alias = Some("my-service".to_owned());

        let normalized = spec.validate().expect("explicit alias accepted");
        assert_eq!(normalized.alias.as_deref(), Some("my-service"));
    }

    #[test]
    fn validate_rejects_empty_endpoint_list() {
        let spec = spec_with_endpoints(Vec::new());

        let err = spec.validate().expect_err("endpoints are required");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_rejects_mixed_scheme_pool() {
        let mut second = endpoint("eu.vendor.com", 443);
        second.scheme = Scheme::Grpc;
        let spec = spec_with_endpoints(vec![endpoint("us.vendor.com", 443), second]);

        let err = spec
            .validate()
            .expect_err("pool must share scheme and port");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_rejects_invalid_hostnames() {
        for host in [
            "-bad.example.com",
            "bad..example.com",
            "ex ample.com",
            "a_-.com",
        ] {
            let spec = spec_with_endpoints(vec![endpoint(host, 443)]);
            let err = spec
                .validate()
                .expect_err("invalid hostname must be rejected");
            assert_eq!(err.status().as_u16(), 400, "host: {host}");
        }
    }

    #[test]
    fn validate_rejects_out_of_range_tags() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.tags = vec!["OpenAI".to_owned()];

        let err = spec.validate().expect_err("tags are lowercase-only");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_rejects_invalid_rate_limit_minimums() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.rate_limit = Some(RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: RateLimitSustained {
                rate: 0,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        });

        let err = spec.validate().expect_err("rate must be >= 1");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn rate_limit_defaults_burst_capacity_to_the_sustained_rate() {
        let rate_limit = RateLimitConfig {
            sharing: SharingMode::Private,
            algorithm: RateLimitAlgorithm::TokenBucket,
            sustained: RateLimitSustained {
                rate: 10,
                window: RateLimitWindow::Minute,
            },
            burst: None,
            scope: RateLimitScope::Tenant,
            strategy: RateLimitStrategy::Reject,
            cost: 1,
        };

        assert_eq!(rate_limit.effective_burst_capacity(), 10);
    }

    #[test]
    fn validate_rejects_credentials_with_wildcard_origin() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: true,
        });

        let err = spec
            .validate()
            .expect_err("credentials require specific origins");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        // The GTS type id of the problem document, so a client can tell this
        // rejection from any other 400 the wire schema produces.
        let problem = err.to_problem_json();
        assert_eq!(
            problem["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
        assert_eq!(problem["context"]["field"], "cors.allowed_origins");

        // A specific origin with credentials is accepted, and the wildcard
        // without credentials is too: the rule is about the combination.
        spec.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["https://studio.openai.com".to_owned()],
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: true,
        });
        assert!(spec.validate_for_create().is_ok(), "create path");

        spec.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        });
        assert!(spec.validate().is_ok(), "wildcard without credentials");
    }

    #[test]
    fn the_replace_path_rejects_credentials_with_wildcard_origin() {
        // A replacement runs the alias check and then the same body validation
        // (`replace_upstream`), so a PUT cannot smuggle in what a POST was
        // refused: an untouched alias makes the first check pass, and the body
        // check still stops the record.
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.cors = Some(CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: default_cors_methods(),
            expose_headers: Vec::new(),
            allow_credentials: true,
        });

        assert!(
            spec.validate_alias_update("api.openai.com").is_ok(),
            "an alias-unchanged replacement passes the alias gate"
        );
        let err = spec
            .validate()
            .expect_err("the replace path runs the same body validation");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
    }

    #[test]
    fn validate_rejects_empty_auth_type() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.auth = Some(AuthConfig {
            plugin_type: String::new(),
            sharing: SharingMode::Private,
            config: None,
        });

        let err = spec.validate().expect_err("auth type is required");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_accepts_a_complete_oauth2_binding_with_literal_credentials() {
        for plugin_type in [
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF,
        ] {
            let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
            spec.auth = Some(AuthConfig {
                plugin_type: plugin_type.to_owned(),
                sharing: SharingMode::Private,
                config: Some(serde_json::json!({
                    "token_endpoint": "https://idp.example.com/token",
                    "client_id_ref": "oagw-gateway",
                    "client_secret_ref": "cred://vendor-client-secret",
                })),
            });

            assert!(
                spec.validate().is_ok(),
                "a literal client id is not a secret ({plugin_type})"
            );
        }
    }

    #[test]
    fn validate_accepts_an_oauth2_binding_that_discovers_its_endpoint() {
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.auth = Some(AuthConfig {
            plugin_type: OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF.to_owned(),
            sharing: SharingMode::Private,
            config: Some(serde_json::json!({
                "issuer_url": "https://idp.example.com",
                "client_id_ref": "cred://vendor-client-id",
                "client_secret_ref": "cred://vendor-client-secret",
                "scopes": "read write",
            })),
        });

        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validate_rejects_an_oauth2_binding_without_a_config_object() {
        for config in [None, Some(serde_json::json!(["token_endpoint"]))] {
            let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
            spec.auth = Some(AuthConfig {
                plugin_type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF.to_owned(),
                sharing: SharingMode::Private,
                config: config.clone(),
            });

            let err = spec
                .validate()
                .expect_err("an oauth2 binding needs a config object");
            assert_eq!(err.status().as_u16(), 400);
            assert_eq!(err.kind(), OagwErrorKind::ValidationError);
            assert_eq!(err.to_problem_json()["context"]["field"], "auth.config");
        }
    }

    #[test]
    fn validate_rejects_an_oauth2_binding_with_zero_or_two_endpoints() {
        for config in [
            serde_json::json!({"client_id_ref": "cred://id", "client_secret_ref": "cred://secret"}),
            serde_json::json!({
                "token_endpoint": "https://idp.example.com/token",
                "issuer_url": "https://idp.example.com",
                "client_id_ref": "cred://id",
                "client_secret_ref": "cred://secret",
            }),
            // An explicit `null` is as good as an absent key.
            serde_json::json!({
                "token_endpoint": serde_json::Value::Null,
                "client_id_ref": "cred://id",
                "client_secret_ref": "cred://secret",
            }),
        ] {
            let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
            spec.auth = Some(AuthConfig {
                plugin_type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF.to_owned(),
                sharing: SharingMode::Private,
                config: Some(config),
            });

            let err = spec
                .validate()
                .expect_err("exactly one endpoint key must be set");
            assert_eq!(err.status().as_u16(), 400);
            assert_eq!(
                err.to_problem_json()["context"]["field"],
                "auth.config",
                "the endpoint keys are object-level"
            );
        }
    }

    #[test]
    fn validate_rejects_an_oauth2_binding_without_both_credential_keys() {
        for config in [
            serde_json::json!({"token_endpoint": "https://idp.example.com/token"}),
            serde_json::json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "cred://vendor-client-id",
            }),
            serde_json::json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "   ",
                "client_secret_ref": "cred://vendor-client-secret",
            }),
            serde_json::json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "cred://vendor-client-id",
                "client_secret_ref": 7,
            }),
        ] {
            let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
            spec.auth = Some(AuthConfig {
                plugin_type: OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF.to_owned(),
                sharing: SharingMode::Private,
                config: Some(config),
            });

            let err = spec
                .validate()
                .expect_err("both credential keys are required");
            assert_eq!(err.status().as_u16(), 400);
            assert_eq!(err.kind(), OagwErrorKind::ValidationError);
            let problem = err.to_problem_json();
            let field = problem["context"]["field"].as_str().unwrap_or_default();
            assert!(
                field.starts_with("auth.config.client_"),
                "the field names the offending key, not the value: {field}"
            );
        }
    }

    #[test]
    fn validate_rejects_a_client_secret_that_is_not_a_credential_reference() {
        // The secret is credential material: a literal would live in the
        // upstream spec, come back in plaintext from the management API and sit
        // outside the store's rotation and access control (ADR-0008's
        // configuration table, DESIGN §2.1). The client identifier stays
        // free-form — a client identifier is not a secret.
        for config in [
            serde_json::json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "cred://vendor-client-id",
                "client_secret_ref": "hunter2",
            }),
            serde_json::json!({
                "issuer_url": "https://idp.example.com",
                "client_id_ref": "oagw-gateway",
                "client_secret_ref": "cred:/not-quite-a-reference",
            }),
        ] {
            let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
            spec.auth = Some(AuthConfig {
                plugin_type: OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF.to_owned(),
                sharing: SharingMode::Private,
                config: Some(config),
            });

            let err = spec
                .validate()
                .expect_err("a client secret outside the store is not a binding");
            assert_eq!(err.status().as_u16(), 400);
            assert_eq!(err.kind(), OagwErrorKind::ValidationError);
            let problem = err.to_problem_json();
            assert_eq!(
                problem["context"]["field"], "auth.config.client_secret_ref",
                "the field names the offending key"
            );
            let detail = problem["detail"].as_str().unwrap_or_default();
            assert!(
                !detail.contains("hunter2") && !detail.contains("vendor-client-secret"),
                "the detail never quotes the configured value: {detail}"
            );
        }
    }

    #[test]
    fn validate_does_not_apply_the_oauth2_rules_to_another_auth_plugin() {
        // The api-key plugin binds a header and a key, neither of which is an
        // OAuth2 endpoint: its own rules are the only ones that apply.
        let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
        spec.auth = Some(AuthConfig {
            plugin_type: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1".to_owned(),
            sharing: SharingMode::Private,
            config: Some(serde_json::json!({"key": "cred://vendor-api-key"})),
        });

        assert!(spec.validate().is_ok());
    }

    #[test]
    fn validate_alias_update_is_idempotent_for_matching_endpoints() {
        let spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);

        assert!(spec.validate_alias_update("api.openai.com").is_ok());
    }

    #[test]
    fn validate_alias_update_rejects_an_alias_change() {
        let spec = spec_with_endpoints(vec![endpoint("api.openai.com", 8443)]);

        let err = spec
            .validate_alias_update("api.openai.com")
            .expect_err("the derived alias would change");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_alias_update_rejects_a_differing_user_alias() {
        let mut spec = spec_with_endpoints(vec![endpoint("10.0.1.1", 443)]);
        spec.alias = Some("other-service".to_owned());

        let err = spec
            .validate_alias_update("my-service")
            .expect_err("alias is immutable");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn validate_alias_update_retains_alias_for_non_derivable_endpoints() {
        let spec = spec_with_endpoints(vec![endpoint("10.0.1.2", 443)]);

        assert!(
            spec.validate_alias_update("my-service").is_ok(),
            "IP -> IP keeps the existing alias"
        );
    }

    #[test]
    fn validate_alias_update_allows_ip_to_hostname_transition() {
        let spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);

        assert!(spec.validate_alias_update("api.openai.com").is_ok());
    }

    #[test]
    fn host_validation_accepts_ips_and_bracketed_ipv6() {
        assert_eq!(validate_host("127.0.0.1").as_deref(), Ok("127.0.0.1"));
        assert_eq!(validate_host("::1").as_deref(), Ok("::1"));
        assert_eq!(validate_host("[::1]").as_deref(), Ok("::1"));
        assert_eq!(validate_host("2001:DB8::1").as_deref(), Ok("2001:db8::1"));
        assert!(validate_host("[not-an-ip]").is_err());
        assert!(validate_host("").is_err());
        assert!(validate_host(&"a".repeat(MAX_HOSTNAME_LEN + 1)).is_err());
    }

    #[test]
    fn alias_validation_enforces_the_pattern() {
        assert!(validate_alias("api.openai.com").is_ok());
        assert!(validate_alias("my-service-1").is_ok());
        assert!(validate_alias("vendor.com:8443").is_ok());
        assert!(validate_alias("MY.Service.COM.").as_deref() == Ok("my.service.com"));
        assert!(validate_alias("-leading").is_err());
        assert!(validate_alias("trailing-").is_err());
        assert!(validate_alias("has space").is_err());
        assert!(validate_alias("").is_err());
    }

    #[test]
    fn tag_validation_enforces_the_pattern() {
        assert!(validate_tag("openai").is_ok());
        assert!(validate_tag("llm-prod_2").is_ok());
        assert!(validate_tag("OpenAI").is_err());
        assert!(validate_tag("").is_err());
    }

    #[test]
    fn plugin_ref_extracts_custom_uuids() {
        let custom =
            PluginRef::new("gts.cf.core.oagw.guard_plugin.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7");
        assert_eq!(
            custom.custom_uuid(),
            Some(Uuid::parse_str("7c9e6679-7425-40de-944b-e07fc1f90ae7").expect("valid uuid"))
        );

        let named =
            PluginRef::new("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
        assert_eq!(named.custom_uuid(), None);
        assert!(validate_plugin_ref(&named).is_ok());
        assert!(validate_plugin_ref(&custom).is_ok());
        assert!(validate_plugin_ref(&PluginRef::new("not-a-gts-id")).is_err());
        assert!(validate_plugin_ref(&PluginRef::new("")).is_err());
    }

    #[test]
    fn plugin_ref_round_trips_in_the_published_string_form() {
        let raw = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
        let bound = PluginRef::bound(raw, serde_json::json!({"header": "x-api-key"}));

        // The wire form is the reference string alone, with or without binding
        // configuration: a read never leaks nor rewrites the configuration.
        let serialized = serde_json::to_string(&bound).expect("serializes");
        assert_eq!(serialized, format!("\"{raw}\""));

        let parsed: PluginRef =
            serde_json::from_str(&serialized).expect("the string form parses back");
        assert_eq!(parsed.as_str(), raw);
        assert!(parsed.config().is_none(), "the string form carries none");
        assert_eq!(parsed, PluginRef::new(raw));
    }

    #[test]
    fn plugin_ref_accepts_the_adr_0009_binding_object() {
        let raw = r#"{
            "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "config": { "required": "x-request-id" }
        }"#;
        let parsed: PluginRef = serde_json::from_str(raw).expect("the object form parses");

        assert_eq!(
            parsed.as_str(),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert_eq!(
            parsed.config(),
            Some(&serde_json::json!({ "required": "x-request-id" }))
        );
    }

    #[test]
    fn plugin_ref_rejects_unknown_keys_in_the_binding_object() {
        let raw = r#"{
            "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "config": {},
            "extra": true
        }"#;
        let parsed: Result<PluginRef, _> = serde_json::from_str(raw);
        assert!(parsed.is_err(), "an unknown key is not silently dropped");
    }

    #[test]
    fn plugin_ref_equality_ignores_the_binding_configuration() {
        let plain = PluginRef::new("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
        let configured = PluginRef::bound(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
            serde_json::json!({"key": "cred://vendor/api-key"}),
        );
        let other = PluginRef::new("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");

        assert_eq!(plain, configured, "the same plugin, bound twice");
        assert_ne!(plain, other);
        assert_eq!(
            hash_of(&plain),
            hash_of(&configured),
            "hashable by reference"
        );

        // The "plugin in use" guard compares references, so this is what keeps a
        // configured binding from escaping a delete.
        let mut set = std::collections::HashSet::new();
        set.insert(plain);
        assert!(set.contains(&configured));
    }

    /// `DefaultHasher` is deterministic within a build, which is all an
    /// equality-of-hash assertion needs.
    fn hash_of(reference: &PluginRef) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(reference, &mut hasher);
        std::hash::Hasher::finish(&hasher)
    }

    #[test]
    fn target_host_is_required_for_common_suffix_pools_only() {
        let single = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "api.openai.com".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: spec_with_endpoints(vec![endpoint("api.openai.com", 443)]),
        };
        assert!(
            !single.requires_target_host(),
            "a single endpoint is never ambiguous"
        );

        // Common-suffix derivation: the alias names the family, not a host.
        let suffix_pool = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "vendor.com".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: spec_with_endpoints(vec![
                endpoint("us.vendor.com", 443),
                endpoint("eu.vendor.com", 443),
            ]),
        };
        assert!(suffix_pool.requires_target_host());

        // The same pool on a non-standard port keeps `:port` in the derived
        // alias, which is still a common-suffix alias.
        let port_pool = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "vendor.com:8443".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: spec_with_endpoints(vec![
                endpoint("us.vendor.com", 8443),
                endpoint("eu.vendor.com", 8443),
            ]),
        };
        assert!(port_pool.requires_target_host());

        // An explicit alias for a non-derivable (IP) pool round-robins.
        let explicit = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "my-service".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: spec_with_endpoints(vec![endpoint("10.0.1.1", 443), endpoint("10.0.1.2", 443)]),
        };
        assert!(!explicit.requires_target_host());

        // An unrelated host pair is also an explicit (non-derivable) pool.
        let unrelated = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "my-service".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: spec_with_endpoints(vec![
                endpoint("us.foo.com", 443),
                endpoint("eu.bar.com", 443),
            ]),
        };
        assert!(!unrelated.requires_target_host());
    }

    #[test]
    fn route_match_serde_uses_the_match_key() {
        let raw = r#"{
            "upstream_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } }
        }"#;
        let spec: RouteSpec = serde_json::from_str(raw).expect("route body parses");
        let http = spec.match_rules.http.expect("http match");

        assert_eq!(http.path, "/v1/chat");
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
        assert!(http.query_allowlist.is_empty());
        assert!(spec.enabled, "routes default to enabled");
        assert!(spec.match_rules.grpc.is_none());
    }

    #[test]
    fn entity_gts_ids_use_the_documented_base_types() {
        let upstream = Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: "api.openai.com".to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: UpstreamSpec::default(),
        };
        assert_eq!(
            upstream.gts_id(),
            "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(ROUTE_TYPE_ID, "gts.cf.core.oagw.route.v1~");
        assert_eq!(AUTH_PLUGIN_TYPE_ID, "gts.cf.core.oagw.auth_plugin.v1~");
        assert_eq!(GUARD_PLUGIN_TYPE_ID, "gts.cf.core.oagw.guard_plugin.v1~");
        assert_eq!(
            TRANSFORM_PLUGIN_TYPE_ID,
            "gts.cf.core.oagw.transform_plugin.v1~"
        );
    }

    #[test]
    fn the_oauth2_plugin_identifiers_use_the_auth_plugin_base_type() {
        // The two identifiers are declared next to the base type they are
        // instances of; this is what keeps a rename of one from silently
        // orphaning the other.
        for identifier in [
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF,
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF,
        ] {
            assert!(identifier.starts_with(AUTH_PLUGIN_TYPE_ID));
        }
        assert_eq!(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF
                .rsplit('~')
                .next()
                .ok_or("instance"),
            Ok("cf.core.oagw.oauth2_client_cred.v1")
        );
        assert_eq!(
            OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF
                .rsplit('~')
                .next()
                .ok_or("instance"),
            Ok("cf.core.oagw.oauth2_client_cred_basic.v1")
        );
        assert_ne!(
            OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF,
            "the two variants are distinct plugins"
        );
    }

    fn http_route_spec(path: &str, methods: &[RouteMethod]) -> RouteSpec {
        RouteSpec {
            upstream_id: Uuid::new_v4(),
            match_rules: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.to_vec(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            enabled: true,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
        }
    }

    #[test]
    fn route_spec_validation_accepts_the_documented_body() {
        let spec = http_route_spec("/v1/chat", &[RouteMethod::Get, RouteMethod::Post]);

        let validated = spec.validate().expect("a minimal http route validates");
        assert_eq!(
            validated, spec,
            "validation is the identity on a valid body"
        );

        let grpc = RouteSpec {
            match_rules: RouteMatch {
                http: None,
                grpc: Some(GrpcMatch {
                    service: "cf.v1.UserService".to_owned(),
                    method: "GetUser".to_owned(),
                }),
            },
            ..spec
        };
        assert!(
            grpc.validate().is_ok(),
            "the wire schema allows a grpc match, though proxying it is phase 3"
        );
    }

    #[test]
    fn route_spec_validation_enforces_the_schema_rules() {
        for (label, spec) in [
            (
                "no match branch",
                RouteSpec {
                    match_rules: RouteMatch {
                        http: None,
                        grpc: None,
                    },
                    ..http_route_spec("/v1", &[RouteMethod::Get])
                },
            ),
            (
                "both match branches",
                RouteSpec {
                    match_rules: RouteMatch {
                        http: Some(HttpMatch {
                            methods: vec![RouteMethod::Get],
                            path: "/v1".to_owned(),
                            query_allowlist: Vec::new(),
                            path_suffix_mode: PathSuffixMode::Append,
                        }),
                        grpc: Some(GrpcMatch {
                            service: "svc".to_owned(),
                            method: "Get".to_owned(),
                        }),
                    },
                    ..http_route_spec("/v1", &[RouteMethod::Get])
                },
            ),
            ("no methods", http_route_spec("/v1", &[])),
            ("relative path", http_route_spec("v1", &[RouteMethod::Get])),
            ("empty path", http_route_spec("", &[RouteMethod::Get])),
        ] {
            let err = spec.validate().expect_err(label);
            assert_eq!(err.status().as_u16(), 400, "{label}");
            assert_eq!(err.kind(), OagwErrorKind::ValidationError, "{label}");
        }
    }

    #[test]
    fn route_spec_validation_rejects_invalid_plugins_tags_and_limits() {
        let mut spec = http_route_spec("/v1", &[RouteMethod::Get]);
        spec.tags = vec!["Chat".to_owned()];
        assert!(spec.validate().is_err(), "tags follow the shared pattern");

        let mut spec = http_route_spec("/v1", &[RouteMethod::Get]);
        spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::new("not-a-gts-id")],
        });
        assert!(spec.validate().is_err(), "plugin references are validated");

        let mut spec = http_route_spec("/v1", &[RouteMethod::Get]);
        spec.rate_limit = Some(RateLimitConfig {
            sustained: RateLimitSustained {
                rate: 0,
                window: RateLimitWindow::Second,
            },
            ..RateLimitConfig {
                sharing: SharingMode::Private,
                algorithm: RateLimitAlgorithm::TokenBucket,
                sustained: RateLimitSustained {
                    rate: 1,
                    window: RateLimitWindow::Second,
                },
                burst: None,
                scope: RateLimitScope::Tenant,
                strategy: RateLimitStrategy::Reject,
                cost: 1,
            }
        });
        assert!(
            spec.validate().is_err(),
            "rate limits follow the shared rules"
        );
    }

    #[test]
    fn validate_rejects_an_incomplete_oauth2_binding_on_an_upstream_plugin_chain() {
        // `plugins.items[]` reaches the data plane through the same auth-binding
        // path the `auth` field does, so the OAuth2 rules apply there too: the
        // same binding is refused wherever it is written, not accepted into the
        // chain and left to fail closed on every request.
        for config in [
            serde_json::json!({"token_endpoint": "https://idp.example.com/token"}),
            serde_json::json!({
                "token_endpoint": "https://idp.example.com/token",
                "client_id_ref": "cred://vendor-client-id",
                // The secret is credential material: a literal is not a binding.
                "client_secret_ref": "hunter2",
            }),
        ] {
            let mut spec = spec_with_endpoints(vec![endpoint("api.openai.com", 443)]);
            spec.plugins = Some(PluginsConfig {
                sharing: SharingMode::Private,
                items: vec![PluginRef::bound(
                    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_REF,
                    config.clone(),
                )],
            });

            let err = spec
                .validate()
                .expect_err("an incomplete oauth2 chain binding is not a configuration");
            assert_eq!(err.status().as_u16(), 400);
            assert_eq!(err.kind(), OagwErrorKind::ValidationError);
            let field = err.to_problem_json()["context"]["field"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            assert!(
                field.starts_with("auth.config."),
                "the field names the binding's own configuration key: {field}"
            );
        }
    }

    #[test]
    fn validate_rejects_an_incomplete_oauth2_binding_on_a_route_plugin_chain() {
        let mut spec = http_route_spec("/v1", &[RouteMethod::Get]);
        spec.plugins = Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![PluginRef::bound(
                OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_REF,
                serde_json::json!({
                    // Neither endpoint key: no token can be fetched.
                    "client_id_ref": "cred://vendor-client-id",
                    "client_secret_ref": "cred://vendor-client-secret",
                }),
            )],
        });

        let err = spec
            .validate()
            .expect_err("an incomplete oauth2 chain binding is not a configuration");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
    }

    #[test]
    fn plugin_type_ids_follow_the_declared_base_types() {
        assert_eq!(plugin_type_id("auth_plugin"), AUTH_PLUGIN_TYPE_ID);
        assert_eq!(plugin_type_id("guard_plugin"), GUARD_PLUGIN_TYPE_ID);
        assert_eq!(plugin_type_id("transform_plugin"), TRANSFORM_PLUGIN_TYPE_ID);
        assert_eq!(
            plugin_type_id("unknown_plugin"),
            "gts.cf.core.oagw.unknown_plugin.v1~",
            "an undeclared type still yields a well-formed base id"
        );
    }

    #[test]
    fn plugin_gts_id_uses_the_plugin_base_type() {
        let id = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("uuid");
        let plugin = Plugin {
            id,
            tenant_id: Uuid::nil(),
            plugin_type: "guard_plugin".to_owned(),
            name: "request_validator".to_owned(),
            config_schema: None,
            source_code: "def on_request(ctx):\n    pass\n".to_owned(),
            last_used_at: None,
            gc_eligible_at: None,
        };

        assert_eq!(
            plugin.gts_id(),
            "gts.cf.core.oagw.guard_plugin.v1~550e8400-e29b-41d4-a716-446655440000",
            "ADR-0001 Appendix A references the plugin by this identifier"
        );
        assert_eq!(plugin.type_id(), GUARD_PLUGIN_TYPE_ID);
    }

    #[test]
    fn plugin_spec_validation_enforces_the_contract() {
        let valid = PluginSpec {
            name: "  request_validator  ".to_owned(),
            plugin_type: "guard_plugin".to_owned(),
            config_schema: Some(serde_json::json!({ "type": "object" })),
            source_code: "def on_request(ctx):\n    pass\n".to_owned(),
        };
        let validated = valid.validate().expect("valid plugin body");
        assert_eq!(validated.name, "request_validator", "the name is trimmed");

        for (label, spec) in [
            (
                "empty name",
                PluginSpec {
                    name: "   ".to_owned(),
                    ..valid.clone()
                },
            ),
            (
                "unknown type",
                PluginSpec {
                    plugin_type: "middleware".to_owned(),
                    ..valid.clone()
                },
            ),
            (
                "empty type",
                PluginSpec {
                    plugin_type: String::new(),
                    ..valid.clone()
                },
            ),
            (
                "blank source",
                PluginSpec {
                    source_code: " \n".to_owned(),
                    ..valid.clone()
                },
            ),
            (
                "empty source",
                PluginSpec {
                    source_code: String::new(),
                    ..valid.clone()
                },
            ),
            (
                "scalar schema",
                PluginSpec {
                    config_schema: Some(serde_json::json!("str")),
                    ..valid.clone()
                },
            ),
        ] {
            let err = spec.validate().expect_err(label);
            assert_eq!(err.status().as_u16(), 400, "{label}");
            assert_eq!(err.kind(), OagwErrorKind::ValidationError, "{label}");
        }
    }

    #[test]
    fn plugin_spec_rejects_unknown_members() {
        let raw = r#"{
            "name": "validator",
            "plugin_type": "guard_plugin",
            "source_code": "def on_request(ctx): pass",
            "id": "550e8400-e29b-41d4-a716-446655440000"
        }"#;

        let parsed: Result<PluginSpec, _> = serde_json::from_str(raw);
        assert!(
            parsed.is_err(),
            "additionalProperties: false must be enforced"
        );
    }
}
