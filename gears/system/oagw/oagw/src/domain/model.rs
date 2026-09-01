// Created: 2026-08-31 by Constructor Tech
//! Domain records and shared configuration value types (DESIGN §3.1).
//!
//! The nested configuration value types are serde-typed here and reused by the
//! REST DTOs (the same pattern `resource-group` applies to its SDK models), so
//! the wire shape and the domain shape cannot drift apart.
//!
//! # Deliberate deviation: unknown members are not rejected
//!
//! None of these payload types use `deny_unknown_fields`. An upstream or route
//! record is versioned with the gear and rolled out to a fleet whose members
//! may run different revisions for a while; a client that sends a member this
//! revision does not know must still be able to manage the rest of the record.
//! Ignoring the unknown member (and echoing nothing for it) keeps that
//! forward-compatible, at the cost of not catching a misspelled member —
//! misspellings of the required members are still caught by the required-field
//! checks.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use crate::domain::plugin::PluginKind;

/// Timestamps attached to every stored record.
///
/// Epoch milliseconds are used because the crate manifest carries no
/// `chrono`/`time` dependency; the values stay comparable for `$orderby`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Timestamps {
    /// Creation instant (epoch milliseconds).
    pub created_at: u64,
    /// Last modification instant (epoch milliseconds).
    pub updated_at: u64,
}

impl Timestamps {
    /// Timestamps for a record created now.
    #[must_use]
    pub fn now() -> Self {
        let now = crate::domain::time::now_millis();
        Self {
            created_at: now,
            updated_at: now,
        }
    }

    /// Timestamps for a record modified now.
    #[must_use]
    pub fn touched(created_at: u64) -> Self {
        Self {
            created_at,
            updated_at: crate::domain::time::now_millis(),
        }
    }
}

/// Upstream connection scheme.
///
/// `http` and `ws` are *plaintext* schemes: they are always legal enum values,
/// and [`Scheme::is_plaintext`] drives the `oagw.config.allow_http_upstream`
/// decision (DESIGN §2.2 `constraint-https-only`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// TLS HTTP.
    Https,
    /// Plaintext WebSocket.
    Ws,
    /// TLS WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// gRPC (HTTP/2).
    Grpc,
}

impl Scheme {
    /// Default port for the scheme (DESIGN §3.2 "Standard ports").
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Scheme::Http | Scheme::Ws => 80,
            Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => 443,
        }
    }

    /// Whether the scheme dials a plaintext connection.
    #[must_use]
    pub const fn is_plaintext(self) -> bool {
        matches!(self, Scheme::Http | Scheme::Ws)
    }

    /// Scheme as it appears in an upstream URL.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
            Scheme::Ws => "ws",
            Scheme::Wss => "wss",
            Scheme::Wt => "wt",
            Scheme::Grpc => "grpc",
        }
    }
}

/// Upstream wire protocol (DESIGN §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum Protocol {
    /// HTTP(S).
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// Canonical GTS identifier for the protocol.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Protocol::Http => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            Protocol::Grpc => "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
        }
    }

    /// Parse a protocol identifier, accepting the canonical GTS id as well as
    /// the short `http` / `grpc` spellings.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1" | "http" => Some(Protocol::Http),
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1" | "grpc" => Some(Protocol::Grpc),
            _ => None,
        }
    }
}

/// Hierarchical configuration sharing mode (PRD §5.5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Visible; descendants may override.
    Inherit,
    /// Visible; descendants may not override.
    Enforce,
}

/// A single upstream endpoint (DESIGN §3.1 `Endpoint`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Endpoint {
    /// Connection scheme.
    pub scheme: Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Resolved port; never `0` once stored. The wire form of an endpoint may
    /// omit it ([`crate::domain::spec::EndpointSpec`]), in which case
    /// [`crate::domain::spec::ServerSpec::endpoints`] materialises the scheme
    /// default before validation and storage.
    pub port: u16,
}

/// Request header transformation rules (upstream schema `headers.request`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeaderRules {
    /// Headers to set (overwrite).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub set: std::collections::HashMap<String, String>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub add: std::collections::HashMap<String, String>,
    /// Header names to remove.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<String>,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules (upstream schema `headers.response`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub set: std::collections::HashMap<String, String>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub add: std::collections::HashMap<String, String>,
    /// Headers to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
}

/// Header transformation configuration (upstream schema `headers`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeadersConfig {
    /// Request-side rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<HeaderRules>,
    /// Response-side rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaderRules>,
}

/// Sustained rate (upstream schema `rate_limit.sustained`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SustainedRate {
    /// Tokens replenished per window.
    pub rate: u64,
    /// Window length.
    #[serde(default = "default_window")]
    pub window: String,
}

const fn default_window() -> String {
    String::new()
}

/// Token bucket / sliding window rate limit configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RateLimitConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm.
    #[serde(default = "default_algorithm")]
    pub algorithm: String,
    /// Sustained rate.
    pub sustained: SustainedRate,
    /// Bucket capacity; defaults to the sustained rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    /// Counter scope.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Behaviour when the limit is exceeded.
    #[serde(default = "default_strategy")]
    pub strategy: String,
    /// Tokens consumed per request.
    #[serde(default = "default_cost")]
    pub cost: u64,
    /// Whether the `X-RateLimit-*` headers are added to the response.
    ///
    /// ADR-0003 "Configuration" lists it with the default `true`; the shipped
    /// upstream schema omits the member, so the model carries the ADR default.
    #[serde(default = "default_response_headers")]
    pub response_headers: bool,
}

const fn default_response_headers() -> bool {
    true
}

/// Burst capacity of the token bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct BurstConfig {
    /// Maximum burst size.
    pub capacity: u64,
}

/// One entry of a plugin chain (`upstream.v1` / `route.v1` `plugins.items[]`).
///
/// The shipped schema spells an entry as a bare reference string
/// (`"items": ["gts.cf.core.oagw.transform_plugin.v1~…"]`). ADR-0009 binds a
/// built-in guard **with** configuration, so an entry may also be an object
/// carrying `plugin_ref` plus the plugin's `config`:
///
/// ```json
/// { "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
///   "config": { "required_request_headers": "x-correlation-id" } }
/// ```
///
/// Accepting both spellings is a strict superset of the shipped schema, so no
/// previously accepted binding is rejected. The object form keeps every member
/// other than `plugin_ref` verbatim, which makes the round trip stable: a body
/// that was parsed and re-serialised parses to the same bytes again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum PluginBinding {
    /// A bare reference: built-in GTS id or custom plugin UUID.
    Reference(String),
    /// A reference plus its plugin configuration (ADR-0009).
    Configured {
        /// Plugin GTS id or custom plugin UUID.
        plugin_ref: String,
        /// Every remaining member, `config` included, preserved verbatim.
        #[serde(flatten)]
        config: serde_json::Map<String, serde_json::Value>,
    },
}

impl PluginBinding {
    /// The plugin reference the binding names.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            PluginBinding::Reference(reference) => reference,
            PluginBinding::Configured { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The `config` object of a configured binding, if it carries one.
    #[must_use]
    pub fn config(&self) -> Option<&serde_json::Map<String, serde_json::Value>> {
        match self {
            PluginBinding::Reference(_) => None,
            PluginBinding::Configured { config, .. } => {
                config.get("config").and_then(serde_json::Value::as_object)
            }
        }
    }
}

/// Plugin chain binding (upstream/route schema `plugins`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PluginsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin references: built-in GTS ids, custom plugin UUIDs or configured
    /// bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

/// CORS configuration (upstream/route schema `cors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CorsConfig {
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS handling is enabled.
    pub enabled: bool,
    /// Allowed origins (`["*"]` allows any).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(
        default = "default_cors_methods",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to browsers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Whether credentials may be sent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_credentials: bool,
}

const fn default_cors_methods() -> Vec<String> {
    Vec::new()
}

/// Auth plugin binding (upstream schema `auth`).
///
/// `raw` keeps every member the caller sent beyond `type`/`sharing` (including
/// the nested `config` object) so the auth plugin configuration round-trips
/// byte-for-byte and stays opaque to the control plane.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AuthConfig {
    /// Auth plugin GTS identifier.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Sharing mode across the tenant hierarchy.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Remaining members, preserved verbatim.
    #[serde(flatten)]
    pub raw: serde_json::Map<String, serde_json::Value>,
}

impl SharingMode {
    /// `true` for [`SharingMode::Private`].
    #[must_use]
    pub const fn is_private(self) -> bool {
        matches!(self, SharingMode::Private)
    }
}

fn default_algorithm() -> String {
    "token_bucket".to_owned()
}

fn default_scope() -> String {
    "tenant".to_owned()
}

fn default_strategy() -> String {
    "reject".to_owned()
}

const fn default_cost() -> u64 {
    1
}

/// Upstream record (`gts.cf.core.oagw.upstream.v1~`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Upstream {
    /// Server-generated UUID (wire `id`).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Routing key, unique per tenant.
    pub alias: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Wire protocol.
    pub protocol: Protocol,
    /// Endpoint pool (all endpoints share scheme and port).
    pub endpoints: Vec<Endpoint>,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation / modification instants.
    pub timestamps: Timestamps,
}

impl Upstream {
    /// Plugin references (auth plugin plus the bound chain).
    ///
    /// Used by the `plugin.in_use` check (DESIGN §3.6 "Track Plugin Usage").
    #[must_use]
    pub fn plugin_references(&self) -> Vec<String> {
        let mut refs = Vec::new();
        if let Some(auth) = &self.auth
            && let Some(plugin_type) = &auth.plugin_type
        {
            refs.push(plugin_type.clone());
        }
        if let Some(plugins) = &self.plugins {
            refs.extend(
                plugins
                    .items
                    .iter()
                    .map(|binding| binding.reference().to_owned()),
            );
        }
        refs
    }
}

/// HTTP request method accepted by a route match rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// GET.
    Get,
    /// POST.
    Post,
    /// PUT.
    Put,
    /// DELETE.
    Delete,
    /// PATCH.
    Patch,
}

impl HttpMethod {
    /// Parse a method name case-insensitively.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "GET" => Some(HttpMethod::Get),
            "POST" => Some(HttpMethod::Post),
            "PUT" => Some(HttpMethod::Put),
            "DELETE" => Some(HttpMethod::Delete),
            "PATCH" => Some(HttpMethod::Patch),
            _ => None,
        }
    }

    /// Canonical uppercase name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Delete => "DELETE",
            HttpMethod::Patch => "PATCH",
        }
    }
}

/// How the proxy path suffix is treated (DESIGN §3.2 "Guard Rules").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject requests carrying a path suffix.
    Disabled,
    /// Append the suffix to the configured path.
    #[default]
    Append,
}

/// HTTP match rule (route schema `match.http`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HttpMatch {
    /// Allowed methods.
    pub methods: Vec<HttpMethod>,
    /// Path pattern.
    pub path: String,
    /// Allowed query parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the proxy path suffix is treated.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl PathSuffixMode {
    /// `true` for [`PathSuffixMode::Append`].
    #[must_use]
    pub const fn is_append(self) -> bool {
        matches!(self, PathSuffixMode::Append)
    }
}

/// gRPC match rule (route schema `match.grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GrpcMatch {
    /// Fully qualified service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

/// Route match rule — exactly one of HTTP / gRPC is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RouteMatch {
    /// HTTP match rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl RouteMatch {
    /// Match key used for the per-upstream uniqueness rule.
    ///
    /// The v1 wire schema carries no `priority`, so the `(path, priority,
    /// method)` tuple of DESIGN §3.6 degenerates to `(path, method)`.
    #[must_use]
    pub fn match_keys(&self) -> Vec<(String, String)> {
        match (&self.http, &self.grpc) {
            (Some(http), _) => http
                .methods
                .iter()
                .map(|method| (http.path.clone(), method.as_str().to_owned()))
                .collect(),
            (None, Some(grpc)) => {
                vec![(grpc.service.clone(), format!("{}/{}", grpc.method, "grpc"))]
            }
            (None, None) => Vec::new(),
        }
    }
}

/// Route record (`gts.cf.core.oagw.route.v1~`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Route {
    /// Server-generated UUID (wire `id`).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream (immutable after creation).
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Match rule.
    pub match_rule: RouteMatch,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation / modification instants.
    pub timestamps: Timestamps,
}

impl Route {
    /// Plugin references bound to this route.
    #[must_use]
    pub fn plugin_references(&self) -> Vec<String> {
        self.plugins.as_ref().map_or_else(Vec::new, |plugins| {
            plugins
                .items
                .iter()
                .map(|binding| binding.reference().to_owned())
                .collect()
        })
    }
}

/// Plugin record (`gts.cf.core.oagw.{type}_plugin.v1~{uuid}`).
///
/// Custom plugins are immutable after creation (DESIGN §3.2 "Plugin Lifecycle
/// Management"), so there is no update path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Plugin {
    /// Server-generated UUID (wire `id`).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin kind (`auth` | `guard` | `transform`).
    pub kind: PluginKind,
    /// Unique name within the tenant.
    pub name: String,
    /// Whether the plugin is enabled.
    pub enabled: bool,
    /// Arbitrary plugin configuration.
    #[serde(default)]
    pub config: serde_json::Value,
    /// JSON Schema describing [`Plugin::config`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Free-text description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Plugin source text (Starlark).
    pub source: String,
    /// Creation / modification instants.
    pub timestamps: Timestamps,
}

impl Plugin {
    /// GTS identifier of this plugin instance.
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", self.kind.gts_type(), self.id)
    }
}
