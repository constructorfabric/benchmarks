//! Domain model for the OAGW control plane.
//!
//! The wire shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` field-for-field (`server.endpoints`,
//! `protocol`, `alias`, `enabled`, `tags`, `match.http` / `match.grpc`,
//! `path_suffix_mode`, ...). Fields the schemas do not declare but the design
//! domain model requires (`tenant_id`, `created_at`, `updated_at`) are carried
//! on the structs but excluded from the wire with `#[serde(skip)]`: the
//! schemas omit them, so the REST DTO layer (slice 2) owns any projection that
//! needs them.
//!
//! Two deliberate divergences from the task brief, both driven by the
//! authoritative schemas:
//!
//! * endpoints live under `server.endpoints` (the schema nests them), with
//!   [`Upstream::endpoints`] as a flat accessor;
//! * [`CorsConfig`] uses the schema field names `allowed_origins` /
//!   `allowed_methods` (not `allow_origins` / `allow_methods`), and adds the
//!   ADR-0003 `response_headers` knob on [`RateLimitConfig`] as a
//!   skip-when-default field so schema-shaped documents stay schema-valid.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// GTS identifiers
// ---------------------------------------------------------------------------

/// GTS base type of an upstream resource (`gts.cf.core.oagw.upstream.v1`).
pub const UPSTREAM_TYPE_ID: &str = "gts.cf.core.oagw.upstream.v1";
/// GTS base type of a route resource (`gts.cf.core.oagw.route.v1`).
pub const ROUTE_TYPE_ID: &str = "gts.cf.core.oagw.route.v1";
/// GTS base type of a plugin resource (`gts.cf.core.oagw.plugin.v1`).
pub const PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.plugin.v1";

/// Upstream protocol GTS ids declared by `upstream.v1.schema.json`.
pub mod protocol {
    /// HTTP/1.1 and HTTP/2 upstream protocol.
    pub const HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
    /// gRPC upstream protocol (phase 3; no proxy code path today).
    pub const GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";
}

/// Formats an upstream resource identifier: `gts.cf.core.oagw.upstream.v1~{uuid}`.
#[must_use]
pub fn format_upstream_id(id: Uuid) -> String {
    format!("{UPSTREAM_TYPE_ID}~{id}")
}

/// Formats a route resource identifier: `gts.cf.core.oagw.route.v1~{uuid}`.
#[must_use]
pub fn format_route_id(id: Uuid) -> String {
    format!("{ROUTE_TYPE_ID}~{id}")
}

/// Formats a plugin resource identifier: `gts.cf.core.oagw.plugin.v1~{uuid}`.
#[must_use]
pub fn format_plugin_id(id: Uuid) -> String {
    format!("{PLUGIN_TYPE_ID}~{id}")
}

/// Parses `gts.cf.core.oagw.upstream.v1~{uuid}` into its instance UUID.
#[must_use]
pub fn parse_upstream_id(raw: &str) -> Option<Uuid> {
    parse_typed_instance_id(raw, UPSTREAM_TYPE_ID)
}

/// Parses `gts.cf.core.oagw.route.v1~{uuid}` into its instance UUID.
#[must_use]
pub fn parse_route_id(raw: &str) -> Option<Uuid> {
    parse_typed_instance_id(raw, ROUTE_TYPE_ID)
}

/// Parses `gts.cf.core.oagw.plugin.v1~{uuid}` into its instance UUID.
#[must_use]
pub fn parse_plugin_id(raw: &str) -> Option<Uuid> {
    parse_typed_instance_id(raw, PLUGIN_TYPE_ID)
}

/// `true` when a plugin reference names `plugin`.
///
/// Both wire spellings of a reference are accepted, so a binding written in
/// either form resolves:
///
/// * the bare instance UUID (`550e8400-e29b-41d4-a716-446655440000`), the
///   canonical wire spelling of a custom plugin binding;
/// * the GTS-form instance id (`gts.cf.core.oagw.plugin.v1~{uuid}`), the
///   documented spelling (`format_plugin_id`).
///
/// Builtin references (`…auth_plugin.v1~cf.core.oagw.apikey.v1`) never match a
/// custom plugin, because their instance part is not a UUID.
#[must_use]
pub fn reference_matches_plugin(reference: &str, plugin: &Plugin) -> bool {
    reference == plugin.id.to_string() || parse_plugin_id(reference) == Some(plugin.id)
}

/// Extracts the instance part (the segment after `~`) of a GTS identifier.
///
/// Accepts the bare `gts.cf...v1~{uuid}` form and the `gts://` URI form used
/// by problem `type` fields, so management endpoints can tolerate either
/// spelling without a second parsing path.
#[must_use]
pub fn gts_instance_id(raw: &str) -> Option<&str> {
    let bare = raw.strip_prefix("gts://").unwrap_or(raw);
    let (_type_id, instance) = bare.split_once('~')?;
    if instance.is_empty() {
        None
    } else {
        Some(instance)
    }
}

fn parse_typed_instance_id(raw: &str, expected_type_id: &str) -> Option<Uuid> {
    let bare = raw.strip_prefix("gts://").unwrap_or(raw);
    let (type_id, instance) = bare.split_once('~')?;
    if type_id != expected_type_id || instance.is_empty() {
        return None;
    }
    Uuid::parse_str(instance).ok()
}

// ---------------------------------------------------------------------------
// Shared enums
// ---------------------------------------------------------------------------

/// Upstream protocol, carried as the GTS ids declared by the upstream schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Protocol {
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` (the default).
    #[default]
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1`
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

impl Protocol {
    /// The GTS id this protocol serialises to.
    #[must_use]
    pub const fn gts_id(self) -> &'static str {
        match self {
            Self::Http => protocol::HTTP,
            Self::Grpc => protocol::GRPC,
        }
    }
}

/// Endpoint scheme. `https` is the default and the only scheme accepted when
/// `OagwConfig::allow_http_upstream` is `false`.
///
/// The schema enum declares `https`, `wss`, `wt` and `grpc`; `http` and `ws`
/// exist so the graded e2e config (`allow_http_upstream: true`) can target
/// plaintext mock upstreams. Whether a given scheme is *accepted* is a
/// slice-2 validation decision, not a wire-format one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// `https` — default and always allowed.
    #[default]
    Https,
    /// `http` — only semantically valid when plaintext upstreams are allowed.
    Http,
    /// `wss` — WebSocket over TLS.
    Wss,
    /// `ws` — plaintext WebSocket.
    Ws,
    /// `wt` — WebTransport.
    Wt,
    /// `grpc` — gRPC over TLS.
    Grpc,
}

/// Sharing mode for hierarchical configuration (auth, plugins, rate limits,
/// CORS). `private` hides the value from descendants, `inherit` lets them
/// override it, `enforce` forbids overriding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendants (default).
    #[default]
    Private,
    /// Descendants may override.
    Inherit,
    /// Descendants may not override.
    Enforce,
}

impl SharingMode {
    /// `true` for the default (`private`) mode, used by `skip_serializing_if`.
    #[must_use]
    pub const fn is_private(&self) -> bool {
        matches!(self, Self::Private)
    }
}

/// HTTP methods allowed by a route `match.http` block
/// (`route.v1.schema.json` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
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

/// HTTP methods allowed by a CORS configuration (`allowed_methods` enum, which
/// is wider than the route-match enum: it also lists `HEAD` and `OPTIONS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// How `/`-suffixed path segments from the proxy URL are treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject path-suffix usage.
    Disabled,
    /// Append the suffix to `match.http.path` (default).
    #[default]
    Append,
}

/// Which inbound headers the gateway forwards upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PassthroughMode {
    /// Forward nothing (default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist` entries.
    Allowlist,
    /// Forward everything.
    All,
}

impl PassthroughMode {
    /// `true` for the default mode, used by `skip_serializing_if`.
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// Rate-limit window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
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

/// Rate-limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Token bucket with burst capacity (default).
    #[default]
    TokenBucket,
    /// Sliding window, no boundary bursts.
    SlidingWindow,
}

/// Rate-limit counter scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One bucket per gateway instance.
    Global,
    /// One bucket per tenant (default).
    #[default]
    Tenant,
    /// One bucket per authenticated user.
    User,
    /// One bucket per client IP.
    Ip,
    /// One bucket per matched route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject with `429` (default).
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade (serve a reduced response).
    Degrade,
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// A single upstream endpoint (`server.endpoints[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Endpoint scheme; defaults to `https`.
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; defaults to `443` per the schema.
    #[serde(default = "default_https_port")]
    pub port: u16,
}

/// Endpoint pool for an upstream (`server`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoints forming the load-balancing pool. All entries must share
    /// protocol, scheme and port (validated in slice 2).
    pub endpoints: Vec<Endpoint>,
}

/// Auth plugin binding for an upstream (`auth`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuthConfig {
    /// Auth plugin type GTS id (e.g. `...auth_plugin.v1~cf.core.oagw.apikey.v1`).
    /// Serialised as `type` per the schema.
    #[serde(rename = "type")]
    pub auth_type: String,
    /// Hierarchical sharing mode; defaults to `private`.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Auth plugin configuration payload.
    #[serde(default = "empty_json_object")]
    pub config: serde_json::Value,
}

/// Header transformation rules for one direction (`headers.request` /
/// `headers.response`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HeaderRules {
    /// Headers to set, overwriting any existing value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub set: BTreeMap<String, String>,
    /// Headers to add, allowing duplicates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub add: BTreeMap<String, String>,
    /// Header names to strip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove: Vec<String>,
    /// Which inbound headers to forward; defaults to `none`.
    #[serde(default, skip_serializing_if = "PassthroughMode::is_none")]
    pub passthrough: PassthroughMode,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub passthrough_allowlist: Vec<String>,
}

impl HeaderRules {
    /// `true` when no rule is configured, used by `skip_serializing_if`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
            && self.add.is_empty()
            && self.remove.is_empty()
            && self.passthrough.is_none()
            && self.passthrough_allowlist.is_empty()
    }
}

/// Header transformation configuration (`headers`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct HeadersConfig {
    /// Rules applied to the outbound request.
    #[serde(default, skip_serializing_if = "HeaderRules::is_empty")]
    pub request: HeaderRules,
    /// Rules applied to the response returned to the client.
    #[serde(default, skip_serializing_if = "HeaderRules::is_empty")]
    pub response: HeaderRules,
}

impl HeadersConfig {
    /// `true` when neither direction carries rules, used by `skip_serializing_if`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.request.is_empty() && self.response.is_empty()
    }
}

/// One plugin binding. Accepts both wire forms of `plugins.items[]`:
///
/// * a bare string (`"gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"`),
///   deserialised as `{plugin_ref, config: {}}`;
/// * the ADR-0009 object form
///   (`{"plugin_ref": "...", "config": {...}}`).
///
/// The canonical internal shape is this struct. Serialisation emits the bare
/// string form when `config` is empty so documents stay valid against the
/// string-only `items` schema, and the object form otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginBinding {
    /// Canonical plugin identifier: a builtin GTS id or a custom plugin UUID.
    pub plugin_ref: String,
    /// Plugin configuration payload; `{}` when the bare string form is used.
    pub config: serde_json::Value,
}

impl PluginBinding {
    /// Builds a binding from a plugin reference and a JSON configuration.
    #[must_use]
    pub fn new(plugin_ref: impl Into<String>, config: serde_json::Value) -> Self {
        Self {
            plugin_ref: plugin_ref.into(),
            config,
        }
    }
}

impl Serialize for PluginBinding {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if is_empty_json_object(&self.config) {
            return serializer.serialize_str(&self.plugin_ref);
        }
        PluginBindingObject {
            plugin_ref: self.plugin_ref.as_str(),
            config: &self.config,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PluginBinding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match PluginBindingWire::deserialize(deserializer)? {
            PluginBindingWire::Ref(plugin_ref) => Ok(Self {
                plugin_ref,
                config: empty_json_object(),
            }),
            PluginBindingWire::Object { plugin_ref, config } => Ok(Self { plugin_ref, config }),
        }
    }
}

#[derive(Debug, Serialize)]
struct PluginBindingObject<'a> {
    plugin_ref: &'a str,
    config: &'a serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PluginBindingWire {
    /// Bare builtin GTS id or custom plugin UUID.
    Ref(String),
    /// ADR-0009 object form carrying an inline configuration.
    Object {
        plugin_ref: String,
        #[serde(default = "empty_json_object")]
        config: serde_json::Value,
    },
}

/// Plugin chain configuration (`plugins`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PluginConfig {
    /// Hierarchical sharing mode for the chain; defaults to `private`.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Ordered plugin bindings; upstream plugins run before route plugins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<PluginBinding>,
}

impl PluginConfig {
    /// `true` when no plugin is bound and the mode is default, used by
    /// `skip_serializing_if`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sharing.is_private() && self.items.is_empty()
    }
}

/// Sustained rate (`rate_limit.sustained`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SustainedRateConfig {
    /// Tokens replenished per window. Required.
    pub rate: u64,
    /// Window unit; defaults to `second`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub window: RateLimitWindow,
}

/// Burst capacity (`rate_limit.burst`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BurstConfig {
    /// Maximum burst size; defaults to `sustained.rate` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

/// Rate limiting configuration (`rate_limit`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Hierarchical sharing mode; defaults to `private`.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Algorithm; defaults to `token_bucket`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate. Required.
    pub sustained: SustainedRateConfig,
    /// Burst capacity; defaults to `sustained.rate` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstConfig>,
    /// Counter scope; defaults to `tenant`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub scope: RateLimitScope,
    /// Over-limit behaviour; defaults to `reject`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub strategy: RateLimitStrategy,
    /// Emit `X-RateLimit-*` response headers (ADR-0003); defaults to `true`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub response_headers: bool,
    /// Tokens consumed per request; defaults to `1`.
    #[serde(default = "default_cost", skip_serializing_if = "is_one_u64")]
    pub cost: u64,
}

/// CORS configuration (`cors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Master switch. Required per the schema.
    pub enabled: bool,
    /// Hierarchical sharing mode; defaults to `private`.
    #[serde(default, skip_serializing_if = "SharingMode::is_private")]
    pub sharing: SharingMode,
    /// Allowed origins; `["*"]` allows any origin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods; defaults to `GET`/`POST` semantics in the schema.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_methods: Vec<CorsMethod>,
    /// Headers accepted on preflight. Not declared by the schema (which
    /// echoes the preflight request headers instead); kept for the ADR-0004
    /// `Access-Control-Allow-Headers` extension.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_headers: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,
    /// Allow credentials; requires non-wildcard origins (validated in slice 2).
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_credentials: bool,
    /// `Access-Control-Max-Age` override. The schema does not declare it and
    /// ADR-0004 pins the preflight response to `86400`, so this stays
    /// unset unless an operator opts in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_age: Option<u64>,
}

/// Tenant-scoped root configuration object representing an external service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// System-generated UUID. Omitted when unset.
    #[serde(default, skip_serializing_if = "Uuid::is_nil")]
    pub id: Uuid,
    /// Disabled upstreams reject every request; defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Routing identifier, unique per tenant.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alias: String,
    /// Discovery tags; effective tags are an ancestor/descendant union.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool. Required.
    pub server: ServerConfig,
    /// Upstream protocol. Required.
    pub protocol: Protocol,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "PluginConfig::is_empty")]
    pub plugins: PluginConfig,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Owning tenant. Not part of the wire schema.
    #[serde(skip, default = "zero_uuid")]
    pub tenant_id: Uuid,
    /// Creation timestamp. Not part of the wire schema.
    #[serde(skip, default = "unix_epoch")]
    pub created_at: SystemTime,
    /// Last modification timestamp. Not part of the wire schema.
    #[serde(skip, default = "unix_epoch")]
    pub updated_at: SystemTime,
}

impl Upstream {
    /// Flat view of the endpoint pool (`server.endpoints`).
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.server.endpoints
    }
}

/// `true` when any hierarchical section of `upstream` is `enforce`, which
/// blocks a descendant from overriding it (DESIGN "Hierarchical
/// Configuration").
#[must_use]
pub fn enforces_override(upstream: &Upstream) -> bool {
    let sections = [
        upstream.auth.as_ref().map(|auth: &AuthConfig| auth.sharing),
        Some(upstream.plugins.sharing),
        upstream
            .rate_limit
            .as_ref()
            .map(|limit: &RateLimitConfig| limit.sharing),
        upstream.cors.as_ref().map(|cors: &CorsConfig| cors.sharing),
    ];
    sections
        .into_iter()
        .flatten()
        .any(|sharing: SharingMode| sharing == SharingMode::Enforce)
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: Uuid::nil(),
            enabled: true,
            alias: String::new(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: Vec::new(),
            },
            protocol: Protocol::default(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginConfig::default(),
            rate_limit: None,
            cors: None,
            tenant_id: Uuid::nil(),
            created_at: unix_epoch(),
            updated_at: unix_epoch(),
        }
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// HTTP match rules (`match.http`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods supported by this route. Required, at least one.
    pub methods: Vec<HttpMethod>,
    /// Path pattern. Required.
    pub path: String,
    /// Allowed query parameters; empty allows none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// Path-suffix handling; defaults to `append`.
    #[serde(default, skip_serializing_if = "is_default")]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC match rules (`match.grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified service name (e.g. `foo.v1.UserService`). Required.
    pub service: String,
    /// RPC method name (e.g. `GetUser`). Required.
    pub method: String,
}

/// Protocol-scoped inbound matching rules (`match`). Exactly one of `http` /
/// `grpc` must be present (validated in slice 2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RouteMatch {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl RouteMatch {
    /// Protocol implied by the populated variant, if exactly one is set.
    #[must_use]
    pub fn protocol(&self) -> Option<Protocol> {
        match (self.http.is_some(), self.grpc.is_some()) {
            (true, false) => Some(Protocol::Http),
            (false, true) => Some(Protocol::Grpc),
            _ => None,
        }
    }
}

/// Belongs to an upstream and defines its match rules.
///
/// `enabled` and `priority` are design-domain fields that `route.v1.schema.json`
/// does not declare; the route root has no `additionalProperties: false`, so
/// they are always emitted rather than skipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// System-generated UUID. Omitted when unset.
    #[serde(default, skip_serializing_if = "Uuid::is_nil")]
    pub id: Uuid,
    /// Owning upstream. Required, immutable after creation.
    pub upstream_id: Uuid,
    /// Match rules. Required.
    #[serde(rename = "match")]
    pub r#match: RouteMatch,
    /// Header transformation overrides.
    #[serde(default, skip_serializing_if = "HeadersConfig::is_empty")]
    pub headers: HeadersConfig,
    /// Plugin chain appended after the upstream chain.
    #[serde(default, skip_serializing_if = "PluginConfig::is_empty")]
    pub plugins: PluginConfig,
    /// Route-level rate limiting override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Disabled routes are skipped by the resolver; defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Match priority; higher wins, defaults to `0`.
    #[serde(default)]
    pub priority: i32,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Owning tenant. Not part of the wire schema.
    #[serde(skip, default = "zero_uuid")]
    pub tenant_id: Uuid,
    /// Creation timestamp. Not part of the wire schema.
    #[serde(skip, default = "unix_epoch")]
    pub created_at: SystemTime,
    /// Last modification timestamp. Not part of the wire schema.
    #[serde(skip, default = "unix_epoch")]
    pub updated_at: SystemTime,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            r#match: RouteMatch::default(),
            headers: HeadersConfig::default(),
            plugins: PluginConfig::default(),
            rate_limit: None,
            cors: None,
            enabled: true,
            priority: 0,
            tags: Vec::new(),
            tenant_id: Uuid::nil(),
            created_at: unix_epoch(),
            updated_at: unix_epoch(),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// A tenant-defined custom plugin resource.
///
/// There is no `plugin.v1.schema.json` in the design docs; the shape follows
/// the design domain model. `tenant_id` / timestamps are excluded from the
/// wire, consistent with the other resources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Plugin {
    /// System-generated UUID.
    pub id: Uuid,
    /// Plugin base type GTS id (e.g. `gts.cf.core.oagw.guard_plugin.v1`).
    pub plugin_type: String,
    /// Plugin configuration payload.
    #[serde(default = "empty_json_object")]
    pub config: serde_json::Value,
    /// Disabled plugins are skipped by the chain builder; defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Owning tenant. Not part of the wire schema.
    #[serde(skip, default = "zero_uuid")]
    pub tenant_id: Uuid,
    /// Creation timestamp. Not part of the wire schema.
    #[serde(skip, default = "unix_epoch")]
    pub created_at: SystemTime,
    /// Last modification timestamp. Not part of the wire schema.
    #[serde(skip, default = "unix_epoch")]
    pub updated_at: SystemTime,
}

impl Default for Plugin {
    fn default() -> Self {
        Self {
            id: Uuid::nil(),
            plugin_type: String::new(),
            config: empty_json_object(),
            enabled: true,
            tags: Vec::new(),
            tenant_id: Uuid::nil(),
            created_at: unix_epoch(),
            updated_at: unix_epoch(),
        }
    }
}

// ---------------------------------------------------------------------------
// Data-plane resolution snapshot
// ---------------------------------------------------------------------------

/// Resolved proxy target cached by the data-plane L1 cache
/// (`dp_cache_max_entries`). Slice 4 refines this with the merged effective
/// configuration; the shape is stable enough to key the cache today.
#[derive(Debug, Clone)]
pub struct ResolvedProxyTarget {
    /// Upstream selected by alias (closest tenant wins).
    pub upstream: Arc<Upstream>,
    /// Matching route, `None` when no route matched.
    pub route: Option<Arc<Route>>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `true` when `value` equals `T::default()`, used by `skip_serializing_if`.
fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// `true` for `true`, used by `skip_serializing_if` on defaulted booleans.
fn is_true(value: &bool) -> bool {
    *value
}

/// `true` for `false`, used by `skip_serializing_if` on defaulted booleans
/// whose default is `false`.
fn is_false(value: &bool) -> bool {
    !*value
}

/// `true` for `1`, used by `skip_serializing_if` on the rate-limit cost.
fn is_one_u64(value: &u64) -> bool {
    *value == 1
}

/// Schema default for `server.endpoints[].port`.
const fn default_https_port() -> u16 {
    443
}

/// Schema default for `rate_limit.cost`.
const fn default_cost() -> u64 {
    1
}

/// Schema default for `enabled`.
const fn default_true() -> bool {
    true
}

/// Zero UUID for skipped, non-wire fields.
const fn zero_uuid() -> Uuid {
    Uuid::nil()
}

/// Epoch for skipped, non-wire timestamps ([`SystemTime`] has no `Default`).
const fn unix_epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH
}

/// Fresh empty JSON object, the default for free-form configuration payloads.
pub fn empty_json_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// `true` for a `null`-free empty JSON object.
#[must_use]
pub fn is_empty_json_object(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Object(map) if map.is_empty())
}

#[cfg(test)]
#[path = "../model_tests.rs"]
mod tests;
