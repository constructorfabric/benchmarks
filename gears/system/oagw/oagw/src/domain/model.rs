//! Upstream entity family, mirroring `docs/schemas/upstream.v1.schema.json`
//! field for field, plus the documented `http`/`ws` scheme extension
//! (`cpt-cf-oagw-dod-schema-validation`).
//!
//! Every nested type here derives both the request (`Deserialize`) and
//! response (`Serialize`) shape via `#[toolkit_macros::api_dto(request,
//! response)]` and, except where noted below, `#[serde(deny_unknown_fields)]`,
//! so a JSON body carrying a property outside the schema's declared set is
//! rejected by `serde` itself rather than by hand-rolled key-set checks.
//! [`AuthConfig`] and [`PluginsConfig`] are the two exceptions
//! (`CODE1-F-003`): their corresponding schema objects (`auth`, `plugins`)
//! declare no `additionalProperties: false`, unlike every sibling object
//! that does, so `deny_unknown_fields` on those two types alone would reject
//! a schema-conformant request.
//!
//! The `Route` entity family below (`cpt-cf-oagw-feature-route-management`)
//! mirrors `docs/schemas/route.v1.schema.json` the same way, reusing this
//! module's `PluginsConfig` and `RateLimitConfig`.

use std::collections::BTreeMap;

use uuid::Uuid;

/// One of the six accepted `server.endpoints[].scheme` values.
///
/// `Https`, `Wss`, `Wt`, and `Grpc` are the schema's checked-in enum;
/// `Http` and `Ws` are the documented extension beyond it (Overview
/// override 2, `cpt-cf-oagw-dod-schema-validation`): a create or replace
/// request carrying `"scheme": "http"` must be accepted, independent of the
/// `allow_http_upstream` data-plane flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum Scheme {
    /// The schema's default scheme.
    Https,
    Wss,
    Wt,
    Grpc,
    /// Plaintext HTTP — the documented extension (override 2).
    Http,
    /// Plaintext WebSocket — the documented extension (override 2).
    Ws,
}

impl Scheme {
    /// The default port for this scheme: `80` for `http`/`ws`, `443` for the
    /// TLS family (`https`, `wss`, `wt`, `grpc`)
    /// (`cpt-cf-oagw-dod-schema-validation`).
    #[must_use]
    pub const fn standard_port(self) -> u16 {
        match self {
            Self::Http | Self::Ws => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }
}

impl Default for Scheme {
    /// The schema's declared default: `https`.
    fn default() -> Self {
        Self::Https
    }
}

/// One upstream server endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Defaults to `https` when omitted.
    #[serde(default)]
    pub scheme: Scheme,
    /// Hostname or IP address of the upstream service.
    pub host: String,
    /// Defaults per-`scheme` (`Scheme::standard_port`) when omitted; always
    /// present in a validated, assembled endpoint.
    pub port: Option<u16>,
}

/// The `server` object: one or more endpoints forming a load-balancing pool.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// `minItems: 1` in the schema; validated in
    /// `crate::domain::validate::validate_semantics`.
    pub endpoints: Vec<Endpoint>,
}

/// The protocol used to connect to the upstream service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum Protocol {
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

/// Sharing mode for hierarchical configuration fields (`auth`, `plugins`,
/// `rate_limit`, `cors`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum Sharing {
    /// Not visible to descendants.
    #[default]
    Private,
    /// Descendants can override.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

/// Authentication configuration for the upstream service.
///
/// No `#[serde(deny_unknown_fields)]` here (`CODE1-F-003`):
/// `upstream.v1.schema.json`'s `auth` object declares no
/// `additionalProperties: false`, unlike every sibling object that does, so
/// an extra property on this object is schema-conformant and must be
/// accepted, not rejected.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct AuthConfig {
    /// Authentication plugin type (GTS identifier), e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
    #[serde(default, rename = "type")]
    pub auth_type: Option<String>,
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// Which inbound headers are forwarded to the upstream by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum Passthrough {
    #[default]
    None,
    Allowlist,
    All,
}

/// Request-header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub add: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    #[serde(default)]
    pub passthrough: Passthrough,
    #[serde(default)]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// Response-header transformation rules.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub add: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub remove: Option<Vec<String>>,
}

/// Header transformation rules for requests and responses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    #[serde(default)]
    pub request: Option<RequestHeaders>,
    #[serde(default)]
    pub response: Option<ResponseHeaders>,
}

/// One plugin binding: either a bare GTS/UUID reference with no attached
/// configuration (the schema's checked-in `oneOf` shape,
/// `docs/schemas/upstream.v1.schema.json`), or a reference carrying a
/// configuration object.
///
/// The object form is a documented, additive extension beyond the checked-in
/// schema (mirroring override 2's `http`/`ws` scheme extension): built-in
/// guard and transform plugins such as `required_headers`
/// (`cpt-cf-oagw-adr-required-headers-guard-plugin`) need per-binding
/// configuration (`required_request_headers`, `required_response_headers`),
/// which the checked-in schema has no room for on a bare string. `#[serde(untagged)]`
/// keeps every existing bare-string binding — the only shape the 337
/// pre-existing tests ever construct — deserializing and serializing
/// identically to a plain JSON string; only a binding that is a JSON object
/// carrying `plugin_ref` uses the second variant.
// Derived directly rather than via `#[toolkit_macros::api_dto(...)]`: that
// macro forces `#[serde(rename_all = "snake_case")]`, which is redundant
// (this type's one struct variant is already snake_case) and the macro has
// no precedent elsewhere in this codebase paired with `#[serde(untagged)]`.
// `utoipa::ToSchema` is derived directly instead, which is all the outer
// `PluginsConfig`'s own `api_dto`-derived `ToSchema` needs from a nested
// field type.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum PluginItem {
    /// A bare reference, config-less.
    Ref(String),
    /// A reference carrying a configuration object (the documented
    /// extension).
    WithConfig {
        plugin_ref: String,
        #[serde(default)]
        config: Option<serde_json::Value>,
    },
}

impl PluginItem {
    /// The GTS or UUID reference string, regardless of which variant this
    /// binding is.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Ref(plugin_ref) | Self::WithConfig { plugin_ref, .. } => plugin_ref,
        }
    }

    /// The attached configuration object, if any.
    #[must_use]
    pub fn config(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Ref(_) => None,
            Self::WithConfig { config, .. } => config.as_ref(),
        }
    }
}

impl From<&str> for PluginItem {
    fn from(plugin_ref: &str) -> Self {
        Self::Ref(plugin_ref.to_owned())
    }
}

impl From<String> for PluginItem {
    fn from(plugin_ref: String) -> Self {
        Self::Ref(plugin_ref)
    }
}

impl PartialEq<str> for PluginItem {
    fn eq(&self, other: &str) -> bool {
        self.plugin_ref() == other
    }
}

/// Plugin chain configuration.
///
/// No `#[serde(deny_unknown_fields)]` here (`CODE1-F-003`): neither
/// `upstream.v1.schema.json`'s nor `route.v1.schema.json`'s `plugins` object
/// declares `additionalProperties: false`, unlike every sibling object that
/// does, so an extra property on this object is schema-conformant and must
/// be accepted, not rejected.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct PluginsConfig {
    #[serde(default)]
    pub sharing: Sharing,
    /// Builtin plugins by GTS identifier, custom plugins by UUID, each
    /// optionally carrying a configuration object ([`PluginItem`]).
    #[serde(default)]
    pub items: Vec<PluginItem>,
}

/// Rate-limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum Algorithm {
    #[default]
    #[serde(rename = "token_bucket")]
    TokenBucket,
    #[serde(rename = "sliding_window")]
    SlidingWindow,
}

/// Time window for the sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum Window {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

/// Sustained rate: tokens replenished per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct Sustained {
    pub rate: u32,
    #[serde(default)]
    pub window: Window,
}

/// Burst (bucket capacity) override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    pub capacity: u32,
}

/// Scope for rate-limit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitScope {
    Global,
    #[default]
    Tenant,
    User,
    Ip,
    Route,
}

/// Behavior when the rate limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum Strategy {
    #[default]
    Reject,
    Queue,
    Degrade,
}

/// Rate-limiting configuration for the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub sharing: Sharing,
    #[serde(default)]
    pub algorithm: Algorithm,
    pub sustained: Sustained,
    #[serde(default)]
    pub burst: Option<Burst>,
    #[serde(default)]
    pub scope: RateLimitScope,
    #[serde(default)]
    pub strategy: Strategy,
    #[serde(default = "default_cost")]
    pub cost: u32,
}

const fn default_cost() -> u32 {
    1
}

/// HTTP method, as accepted by `cors.allowed_methods`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum HttpMethod {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
    #[serde(rename = "PUT")]
    Put,
    #[serde(rename = "PATCH")]
    Patch,
    #[serde(rename = "DELETE")]
    Delete,
    #[serde(rename = "HEAD")]
    Head,
    #[serde(rename = "OPTIONS")]
    Options,
}

fn default_allowed_methods() -> Vec<HttpMethod> {
    vec![HttpMethod::Get, HttpMethod::Post]
}

impl HttpMethod {
    /// The wire-form HTTP method spelling (`GET`, `POST`, ...), used by
    /// `cpt-cf-oagw-algo-guard-evaluation`'s CORS method check
    /// (`cpt-cf-oagw-dod-cors-request-enforcement`).
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

/// CORS configuration for the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    #[serde(default)]
    pub sharing: Sharing,
    pub enabled: bool,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_allowed_methods")]
    pub allowed_methods: Vec<HttpMethod>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

/// The wildcard CORS origin value.
pub const WILDCARD_ORIGIN: &str = "*";

/// A stored / returned upstream: the full representation, including the
/// server-generated `id` and the resolved `alias`
/// (`cpt-cf-oagw-dod-create-upstream-endpoint`,
/// `cpt-cf-oagw-dod-replace-upstream-endpoint`).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct Upstream {
    pub id: Uuid,
    pub enabled: bool,
    pub alias: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// The wire shape of a create (`POST`) or replace (`PUT`) request body.
///
/// `id` is accepted (it is a declared schema property, just `readOnly`) but
/// ignored — the server always generates its own identifier. `alias` and
/// `enabled` are optional here even though they are always present on the
/// stored/returned [`Upstream`]: `alias` may need to be derived, and
/// `enabled` defaults to `true`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    #[serde(default, rename = "id")]
    pub client_supplied_id: Option<Uuid>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

// ---------------------------------------------------------------------------
// Route entity family, mirroring `docs/schemas/route.v1.schema.json` field
// for field, plus the `enabled`/`priority` application-level extension
// (`cpt-cf-oagw-dod-enable-disable-fields`, DECOMPOSITION.md §2.3).
// ---------------------------------------------------------------------------

/// HTTP method accepted in a route's `match.http.methods`
/// (`cpt-cf-oagw-dod-http-match-validation`).
///
/// A narrower, route-specific enum than [`HttpMethod`] (used for
/// `cors.allowed_methods`): `route.v1.schema.json`'s `http_match.methods`
/// enum is `GET, POST, PUT, DELETE, PATCH` only — no `HEAD` or `OPTIONS` — so
/// this dedicated type lets `serde` itself reject an out-of-enum method
/// value, rather than accepting it and checking it separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum RouteMethod {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
    #[serde(rename = "PUT")]
    Put,
    #[serde(rename = "DELETE")]
    Delete,
    #[serde(rename = "PATCH")]
    Patch,
}

impl RouteMethod {
    /// The wire-form HTTP method spelling (`GET`, `POST`, ...), used by
    /// `cpt-cf-oagw-algo-http-route-select` to compare a route's
    /// `match.http.methods` against the inbound request's method string.
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

/// How a proxy URL's trailing `/{path_suffix}` segment is treated
/// (`cpt-cf-oagw-dod-http-match-validation`): `append` (the schema default)
/// appends it to `path`; `disabled` rejects `path_suffix` usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub enum PathSuffixMode {
    #[default]
    Append,
    Disabled,
}

/// HTTP-scoped match rules (`cpt-cf-oagw-dod-http-match-validation`).
///
/// `methods`' `minItems: 1` and `path`'s `minLength: 1` are validated in
/// `crate::domain::route_validate`, not expressible as `serde` types alone.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    pub methods: Vec<RouteMethod>,
    pub path: String,
    /// Defaults to an empty array when omitted; empty means allow none
    /// (`cpt-cf-oagw-dod-http-match-validation`).
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC-scoped match rules (`cpt-cf-oagw-dod-grpc-match-validation`).
///
/// Accepted, validated, and stored only: no gRPC proxy code path exists at
/// runtime (Overview override 5). `service` and `method`'s `minLength: 1`
/// are validated in `crate::domain::route_validate`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    pub service: String,
    pub method: String,
}

/// Protocol-scoped inbound matching rules: exactly one of `http` or `grpc`
/// must be present (`cpt-cf-oagw-dod-exactly-one-match`,
/// `cpt-cf-oagw-algo-exactly-one-match-enforcement`).
///
/// `serde` alone can only reject an unknown key here (`additionalProperties:
/// false`); the "exactly one of" rule is enforced in
/// `crate::domain::route_validate::validate_match`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    #[serde(default)]
    pub http: Option<HttpMatch>,
    #[serde(default)]
    pub grpc: Option<GrpcMatch>,
}

/// A stored / returned route: the full representation, including the
/// server-generated `id` (`cpt-cf-oagw-dod-route-crud-endpoints`).
///
/// `enabled` and `priority` are application-level fields
/// (`cpt-cf-oagw-dod-enable-disable-fields`) that DESIGN.md and PRD.md
/// define but `route.v1.schema.json`'s declared properties
/// (`id, tags, upstream_id, match, plugins, rate_limit`) do not include;
/// they are accepted and persisted without ever being checked against the
/// schema.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct Route {
    pub id: Uuid,
    pub upstream_id: Uuid,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    pub enabled: bool,
    pub priority: i64,
}

/// The wire shape of a create (`POST`) or replace (`PUT`) route request
/// body.
///
/// `id` is accepted (it is a declared schema property, just `readOnly`) but
/// ignored — the server always generates its own identifier. `upstream_id`
/// is `Option` here even though the schema marks it required: a create
/// request must supply it (checked in
/// `crate::domain::service::create_route`), while a replace request may
/// omit it to retain the stored value unchanged
/// (`cpt-cf-oagw-dod-upstream-id-immutability`). `match` is likewise
/// `Option` here so a request omitting both `upstream_id` and `match` can
/// be rejected with a single 400 naming both missing fields, rather than
/// failing fast on whichever field `serde` would reject first.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    #[serde(default, rename = "id")]
    pub client_supplied_id: Option<Uuid>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    #[serde(default, rename = "match")]
    pub match_config: Option<MatchConfig>,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Application-level field (`cpt-cf-oagw-dod-enable-disable-fields`):
    /// defaults to `true` when omitted, never validated against
    /// `route.v1.schema.json`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Application-level field (`cpt-cf-oagw-dod-enable-disable-fields`):
    /// defaults to `0` when omitted, never validated against
    /// `route.v1.schema.json`.
    #[serde(default)]
    pub priority: Option<i64>,
}

// ---------------------------------------------------------------------------
// Plugin entity family (`cpt-cf-oagw-feature-plugin-management`), covering
// custom, UUID-backed plugin definitions only. Named built-in plugins are
// resolved from an in-process registry (`crate::domain::plugin_resolve`) and
// are never represented by these types or persisted in
// `crate::state::TenantState::plugins`.
// ---------------------------------------------------------------------------

/// One of the three plugin kinds (`cpt-cf-oagw-adr-plugin-system`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum PluginType {
    Auth,
    Guard,
    Transform,
}

impl PluginType {
    /// The wire-form / GTS-segment spelling of this plugin type: `auth`,
    /// `guard`, or `transform`.
    #[must_use]
    pub const fn wire_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }

    /// The phase set permitted for this plugin type
    /// (`cpt-cf-oagw-dod-plugin-config-schema-validation`,
    /// `cpt-cf-oagw-adr-plugin-system`): `guard` permits `on_request` and
    /// `on_response`; `transform` permits all three phases; `auth` permits
    /// none at all.
    #[must_use]
    pub const fn permitted_phases(self) -> &'static [Phase] {
        match self {
            Self::Auth => &[],
            Self::Guard => &[Phase::OnRequest, Phase::OnResponse],
            Self::Transform => &[Phase::OnRequest, Phase::OnResponse, Phase::OnError],
        }
    }
}

/// A plugin execution phase (`cpt-cf-oagw-adr-plugin-system`).
///
/// The shared `on_` prefix mirrors the wire-form phase names
/// (`on_request`/`on_response`/`on_error`) exactly, so it is kept rather
/// than renamed to satisfy `clippy::enum_variant_names`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
#[toolkit_macros::api_dto(request, response)]
pub enum Phase {
    OnRequest,
    OnResponse,
    OnError,
}

/// Builds the wire-form GTS plugin identifier
/// `gts.cf.core.oagw.{type}_plugin.v1~{instance}`
/// (`cpt-cf-oagw-dod-plugin-identification`).
#[must_use]
pub fn gts_plugin_id(plugin_type: PluginType, instance: impl std::fmt::Display) -> String {
    format!(
        "gts.cf.core.oagw.{}_plugin.v1~{instance}",
        plugin_type.wire_str()
    )
}

/// Parses a GTS plugin identifier into its `{type}` and instance parts
/// (`cpt-cf-oagw-algo-plugin-ref-resolution`). Returns `None` when the
/// identifier carries no `~` separator, or its base type is not one of
/// `auth`, `guard`, `transform`.
#[must_use]
pub fn parse_gts_plugin_ref(gts_ref: &str) -> Option<(PluginType, &str)> {
    let (base, instance) = gts_ref.split_once('~')?;
    let plugin_type = match base {
        "gts.cf.core.oagw.auth_plugin.v1" => PluginType::Auth,
        "gts.cf.core.oagw.guard_plugin.v1" => PluginType::Guard,
        "gts.cf.core.oagw.transform_plugin.v1" => PluginType::Transform,
        _ => return None,
    };
    Some((plugin_type, instance))
}

/// A stored custom plugin definition, including its source text
/// (`cpt-cf-oagw-dod-plugin-create`).
///
/// Deliberately not `Serialize`: the API-facing [`Plugin`] view and
/// [`PluginSource`] response are built from this type's fields explicitly,
/// so `source_code` can never leak onto the wrong response by accident.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPlugin {
    pub id: Uuid,
    pub plugin_type: PluginType,
    pub name: String,
    pub config_schema: serde_json::Value,
    pub phases: Vec<Phase>,
    pub source_code: String,
}

/// A stored plugin, as returned by `POST` and `GET` (list and by-id)
/// (`cpt-cf-oagw-dod-plugin-create`, `cpt-cf-oagw-dod-plugin-get`,
/// `cpt-cf-oagw-dod-plugin-list`).
///
/// Deliberately excludes `source_code`
/// (`cpt-cf-oagw-dod-plugin-get-source`): the stored source is retrievable
/// only from [`PluginSource`] via `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct Plugin {
    /// The wire-form GTS identifier,
    /// `gts.cf.core.oagw.{type}_plugin.v1~{uuid}`
    /// (`cpt-cf-oagw-dod-plugin-identification`).
    pub id: String,
    /// Named `kind` (not `plugin_type`) to avoid a field name repeating the
    /// enclosing struct's own name (`clippy::struct_field_names`); the wire
    /// field name stays `plugin_type`, matching `StoredPlugin`'s own field
    /// and the documented `/oagw/v1/plugins` response shape.
    #[serde(rename = "plugin_type")]
    pub kind: PluginType,
    pub name: String,
    pub config_schema: serde_json::Value,
    pub phases: Vec<Phase>,
}

/// The stored source text of a UUID-backed custom plugin
/// (`cpt-cf-oagw-dod-plugin-get-source`).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSource {
    /// The wire-form GTS identifier of the resolved plugin.
    pub id: String,
    pub source_code: String,
}

/// The wire shape of a create (`POST`) plugin request body
/// (`cpt-cf-oagw-dod-plugin-create`).
///
/// There is no replace/update request type: plugin definitions are
/// immutable after creation (`cpt-cf-oagw-dod-plugin-no-replace`) — changing
/// behavior means creating a new plugin and rebinding upstream/route
/// references to it.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    pub plugin_type: PluginType,
    pub name: String,
    pub config_schema: serde_json::Value,
    #[serde(default)]
    pub phases: Vec<Phase>,
    pub source_code: String,
}
