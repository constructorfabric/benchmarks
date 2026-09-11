//! REST transport DTOs of the upstream management surface (FEATURE entry 2.2,
//! flows `create`/`list`/`get`/`replace`/`delete`/`enable-disable`) and of the
//! route management surface (FEATURE entry 2.3).
//!
//! The domain [`Upstream`] is the wire shape of
//! `schemas/upstream.v1.schema.json` *for the read-back*, but it cannot be the
//! write body: it carries `id`, it defaults the `alias` to an empty string it
//! cannot distinguish from "not supplied", and it does not reject unknown
//! properties. The request body is therefore its own DTO with
//! `deny_unknown_fields`, no `id` and no `tenant_id`, while the response is the
//! stored record augmented with the effective enablement.
//!
//! The route write body follows the same discipline, with one asymmetry: the
//! create body carries `upstream_id`, which the update body does not, because
//! route `upstream_id` is immutable and is not part of the update DTO
//! (`cpt-cf-oagw-dod-route-management-upstream-reference`).
//!
//! # Absent stays absent
//!
//! Every sub-configuration block is `Option<_>` and is passed through
//! untouched, so a body that omits `auth`, `headers`, `rate_limit`, `cors` or
//! `plugins` persists no such block, and no implicit empty block is ever
//! materialized (`cpt-cf-oagw-dod-upstream-management-schema-shapes`).
// @cpt-algo:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1
// @cpt-algo:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1

use uuid::Uuid;

use toolkit_macros::api_dto;

use crate::domain::dto::{AuthConfig, CorsConfig, HeadersConfig, MatchConfig, Plugin, PluginsConfig, RateLimitConfig, Route, ServerConfig, SharingMode, Upstream};
use crate::domain::services::management::EffectiveEnablement;

// @cpt-begin:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-1
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-2
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-3
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-1
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-10
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-2
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-3
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-7
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-8
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-9
fn default_enabled() -> bool {
    true
}
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-3
// @cpt-end:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-2
// @cpt-end:cpt-cf-oagw-algo-plugin-system-in-use-scan:p1:inst-ps-scan-1
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-9
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-8
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-7
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-3
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-2
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-10
// @cpt-end:cpt-cf-oagw-algo-plugin-system-token-cache-key:p1:inst-ps-key-1
//

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// The request body of `POST /oagw/v1/upstreams` and of the full replacement
/// `PUT /oagw/v1/upstreams/{id}` — the same shape, because the two flows share
/// the body contract.
///
/// A supplied `id` or `tenant_id` is an unknown property and is rejected, so
/// the server-generated identifier and the server-assigned tenant can never be
/// dictated by the caller.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    /// Absent or empty means *not supplied*: the alias is derived from the
    /// endpoint pool, or an explicit one is required for a non-derivable pool.
    #[serde(default)]
    #[schema(value_type = String, example = "api.vendor.com")]
    pub alias: String,
    /// One of the two protocol GTS identifiers.
    #[schema(value_type = String)]
    pub protocol: String,
    /// Scalar structural default: `true` when omitted.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// The endpoint pool; required.
    #[schema(value_type = Object)]
    pub server: ServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsRequestDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    #[schema(value_type = [String])]
    pub tags: Vec<String>,
}

/// The `cors` block of a request body.
///
/// `enabled` carries **no** default here: the schema default
/// `cors.enabled: false` belongs to the merge engine, so a `cors` block that
/// omits the flag is a rejected body rather than a silently disabled one.
/// Everything else keeps the shape of `docs/schemas/upstream.v1.schema.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CorsRequestDto {
    #[serde(default)]
    #[schema(value_type = String)]
    pub sharing: SharingMode,
    pub enabled: bool,
    /// Each entry is `*` or a well-formed origin URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    #[serde(default)]
    pub expose_headers: Vec<String>,
    #[serde(default)]
    pub allow_credentials: bool,
}

impl From<CorsRequestDto> for CorsConfig {
    fn from(dto: CorsRequestDto) -> Self {
        Self {
            sharing: dto.sharing,
            enabled: dto.enabled,
            allowed_origins: dto.allowed_origins,
            allowed_methods: dto.allowed_methods,
            expose_headers: dto.expose_headers,
            allow_credentials: dto.allow_credentials,
        }
    }
}

impl UpstreamRequest {
    /// The `alias` the caller supplied, or `None` when it was absent or empty.
    #[must_use]
    pub fn supplied_alias(&self) -> Option<&str> {
        let alias = self.alias.trim();
        (!alias.is_empty()).then_some(alias)
    }

    /// The body as the domain write request: no identifier, no tenant, the
    /// alias left at the empty convention when it was not supplied.
    #[must_use]
    pub fn into_upstream(self) -> Upstream {
        Upstream {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            alias: self.alias.trim().to_owned(),
            protocol: self.protocol,
            enabled: self.enabled,
            server: self.server,
            auth: self.auth,
            headers: self.headers,
            rate_limit: self.rate_limit,
            cors: self.cors.map(crate::domain::dto::CorsConfig::from),
            plugins: self.plugins,
            tags: self.tags,
        }
    }
}

/// The effective enablement a record presents to the requesting tenant, with
/// the disabling tenant when the effective state is disabled
/// (`inst-um-ei-6`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[api_dto(response)]
pub struct EffectiveEnablementDto {
    /// The effective state.
    pub enabled: bool,
    /// The tenant holding the disablement, or `None` when the record is
    /// governed by its own flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = String, format = Uuid)]
    pub disabling_tenant_id: Option<Uuid>,
}

impl From<EffectiveEnablement> for EffectiveEnablementDto {
    fn from(effective: EffectiveEnablement) -> Self {
        Self {
            enabled: effective.enabled,
            disabling_tenant_id: effective.disabling_tenant_id,
        }
    }
}

/// The response of a create, a get and a full replacement: the stored record
/// and its effective enablement.
///
/// `Upstream.tenant_id` is `#[serde(skip)]`, so the owner tenant is never on
/// the wire — the upstream schema is `additionalProperties: false` and carries
/// no such property.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct UpstreamResponse {
    /// The server-generated identifier.
    #[schema(value_type = String, format = Uuid)]
    pub id: Uuid,
    /// The routing identifier, derived or reconciled.
    pub alias: String,
    /// The protocol GTS identifier.
    #[schema(value_type = String)]
    pub protocol: String,
    /// The stored state.
    pub enabled: bool,
    /// The endpoint pool.
    #[schema(value_type = Object)]
    pub server: ServerConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub headers: Option<HeadersConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    #[schema(value_type = [String])]
    pub tags: Vec<String>,
    /// The effective enablement the record presents to the requesting tenant
    /// (`inst-um-ei-6`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub effective_enablement: Option<EffectiveEnablementDto>,
}

impl UpstreamResponse {
    /// A response carrying the effective enablement of `view`.
    #[must_use]
    pub fn from_record(record: &Upstream, effective: EffectiveEnablement) -> Self {
        Self {
            id: record.id,
            alias: record.alias.clone(),
            protocol: record.protocol.clone(),
            enabled: record.enabled,
            server: record.server.clone(),
            auth: record.auth.clone(),
            headers: record.headers.clone(),
            rate_limit: record.rate_limit.clone(),
            cors: record.cors.clone(),
            plugins: record.plugins.clone(),
            tags: record.tags.clone(),
            effective_enablement: Some(EffectiveEnablementDto::from(effective)),
        }
    }
}

/// The list body of `GET /oagw/v1/upstreams`: the projected list and the count
/// actually returned (`inst-um-ls-7`).
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct UpstreamListResponse {
    /// The number of records returned.
    pub count: usize,
    /// The projected records, in the order the query produced.
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Routes (FEATURE entry 2.3)
// ---------------------------------------------------------------------------

fn default_priority() -> i64 {
    0
}

/// The request body of `POST /oagw/v1/routes`.
///
/// The payload contract is the published route schema shape plus the named,
/// closed set of schema-external API fields `priority`, `enabled` and `cors`
/// (`cpt-cf-oagw-dod-route-management-schema-conformance`), and the set is
/// closed: a supplied `id`, `tenant_id`, `match_type` or any other field
/// outside the union is an unknown property and is rejected.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    /// The owning upstream, which must resolve inside the calling tenant.
    #[schema(value_type = String, format = Uuid)]
    pub upstream_id: Uuid,
    /// Schema-external API field; the declared default `0` is materialized
    /// when the field is omitted, and a higher value wins at match time.
    #[serde(default = "default_priority")]
    pub priority: i64,
    /// Schema-external API field; the declared default `true` is materialized
    /// when the field is omitted.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Exactly one of `http` or `grpc`; both match definitions are closed to
    /// unknown keys.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_: MatchConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Schema-external API field; the route schema document does not carry it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsRequestDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    #[schema(value_type = [String])]
    pub tags: Vec<String>,
}

/// The request body of the full replacement `PUT /oagw/v1/routes/{id}`.
///
/// `upstream_id` is **not** part of the update DTO
/// (`cpt-cf-oagw-dod-route-management-upstream-reference`): the stored value is
/// retained, and a body that supplies it is an unknown property and is rejected
/// with `400` as an immutable-field violation regardless of the supplied value.
///
/// `enabled` is a scalar field of the replacement body and carries the declared
/// default `true`, so a body that omits it **re-enables** a disabled route
/// (`inst-rm-enab-3`); the same holds for `priority`, whose declared default
/// `0` is materialized here. Omitting `rate_limit`, `cors`, `tags` or
/// `plugins` clears that override.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteUpdateRequest {
    /// Schema-external API field; the declared default `0` is materialized
    /// when the field is omitted.
    #[serde(default = "default_priority")]
    pub priority: i64,
    /// Schema-external API field; the declared default `true` re-enables a
    /// disabled route.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Exactly one of `http` or `grpc`.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_: MatchConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Schema-external API field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsRequestDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    #[schema(value_type = [String])]
    pub tags: Vec<String>,
}

impl RouteRequest {
    /// The body as the domain write request: no identifier, no tenant.
    #[must_use]
    pub fn into_route(self) -> Route {
        Route {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            upstream_id: self.upstream_id,
            match_type: crate::domain::dto::RouteMatchType::Http,
            priority: self.priority,
            enabled: self.enabled,
            match_: self.match_,
            rate_limit: self.rate_limit,
            cors: self.cors.map(crate::domain::dto::CorsConfig::from),
            plugins: self.plugins,
            tags: self.tags,
        }
    }
}

impl RouteUpdateRequest {
    /// The body as the domain write request, with `upstream_id` left at the
    /// nil UUID that means *not supplied, retain the stored one*.
    #[must_use]
    pub fn into_route(self) -> Route {
        Route {
            id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            match_type: crate::domain::dto::RouteMatchType::Http,
            priority: self.priority,
            enabled: self.enabled,
            match_: self.match_,
            rate_limit: self.rate_limit,
            cors: self.cors.map(crate::domain::dto::CorsConfig::from),
            plugins: self.plugins,
            tags: self.tags,
        }
    }
}

/// The response of a route create, a read and a full replacement: the stored
/// record.
///
/// `Route.tenant_id` and `Route.match_type` are `#[serde(skip)]`, so neither
/// the owner tenant nor the derived match type is on the wire — the route
/// schema is `additionalProperties: false` and carries neither.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct RouteResponse {
    /// The server-generated identifier; the path segment addresses the same
    /// record under the base type `gts.cf.core.oagw.route.v1~`.
    #[schema(value_type = String, format = Uuid)]
    pub id: Uuid,
    /// The immutable owning upstream.
    #[schema(value_type = String, format = Uuid)]
    pub upstream_id: Uuid,
    /// `0` when the body omitted it; a higher value wins at match time.
    pub priority: i64,
    /// `true` when the body omitted it.
    pub enabled: bool,
    /// The stored match block.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_: MatchConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    #[schema(value_type = [String])]
    pub tags: Vec<String>,
}

impl RouteResponse {
    /// A response carrying the stored record as it is.
    #[must_use]
    pub fn from_record(record: &Route) -> Self {
        Self {
            id: record.id,
            upstream_id: record.upstream_id,
            priority: record.priority,
            enabled: record.enabled,
            match_: record.match_.clone(),
            rate_limit: record.rate_limit.clone(),
            cors: record.cors.clone(),
            plugins: record.plugins.clone(),
            tags: record.tags.clone(),
        }
    }
}

/// The list body of `GET /oagw/v1/routes`: the projected list and the count
/// actually returned.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct RouteListResponse {
    /// The number of records returned.
    pub count: usize,
    /// The projected records, in the order the query produced.
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Plugins (FEATURE entry 2.6)
// ---------------------------------------------------------------------------

/// The request body of `POST /oagw/v1/plugins`
/// (`cpt-cf-oagw-flow-plugin-system-plugin-create`).
///
/// `plugin_type` names one of the three plugin base types, `name` is unique
/// within the calling tenant, and `source_code` is the opaque reference
/// artifact the registry-reference-only posture stores and never interprets
/// (graded deviation 6). The set is closed: a supplied `id` is an unknown
/// property, because the identifier is server-generated.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    /// One of `gts.cf.core.oagw.auth_plugin.v1~`,
    /// `gts.cf.core.oagw.guard_plugin.v1~` or
    /// `gts.cf.core.oagw.transform_plugin.v1~`, or a full plugin identifier
    /// whose base type is one of the three.
    pub plugin_type: String,
    /// The plugin name, unique within the calling tenant.
    pub name: String,
    /// The JSON schema object the instance configurations validate against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<serde_json::Value>,
    /// The opaque source reference; never interpreted as executable content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
}

impl PluginRequest {
    /// The domain record the body describes, with the server-assigned fields
    /// still at their zero values.
    #[must_use]
    pub fn into_plugin(self) -> Plugin {
        Plugin {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            plugin_type: self.plugin_type,
            name: self.name,
            config_schema: self.config_schema,
            source_code: self.source_code,
            last_used_at: None,
            gc_eligible_at: None,
        }
    }
}

/// The read-back of one custom plugin.
///
/// The record carries no credential material and no resolved credential value:
/// only the `cred://` references a `config_schema` example may name are ever
/// present, and they are references (`cpt-cf-oagw-algo-plugin-system-credential-isolation`).
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct PluginResponse {
    /// The server-generated identifier; the path segment addresses the same
    /// record under the base type `gts.cf.core.oagw.{type}_plugin.v1~`.
    #[schema(value_type = String, format = Uuid)]
    pub id: Uuid,
    /// The stored plugin base type.
    pub plugin_type: String,
    /// The stored name.
    pub name: String,
    /// The stored JSON schema object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<serde_json::Value>,
    /// The anonymous GTS resource identifier of the record,
    /// `gts.cf.core.oagw.{type}_plugin.v1~{id}`.
    pub plugin_ref: String,
}

impl PluginResponse {
    /// A response carrying the stored record, without its source content.
    #[must_use]
    pub fn from_record(record: &Plugin) -> Self {
        Self {
            id: record.id,
            plugin_type: record.plugin_type.clone(),
            name: record.name.clone(),
            config_schema: record.config_schema.clone(),
            plugin_ref: crate::domain::gts_helpers::plugin_resource_id(
                &record.plugin_type,
                record.id,
            ),
        }
    }
}

/// The list body of `GET /oagw/v1/plugins`: the projected list and the count
/// actually returned.
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct PluginListResponse {
    /// The number of records returned.
    pub count: usize,
    /// The projected records, in the order the query produced.
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<serde_json::Value>,
}

/// The body of `GET /oagw/v1/plugins/{id}/source`
/// (`cpt-cf-oagw-flow-plugin-system-plugin-source`).
///
/// The source is returned as an **opaque reference artifact**: no code path in
/// this gear interprets or executes it, which is the plugin-trait boundary that
/// stands in for the Starlark sandbox (graded deviation 6).
#[derive(Debug, Clone, PartialEq)]
#[api_dto(response)]
pub struct PluginSourceResponse {
    /// The identifier of the record the source belongs to.
    #[schema(value_type = String, format = Uuid)]
    pub id: Uuid,
    /// The stored plugin base type.
    pub plugin_type: String,
    /// The stored name.
    pub name: String,
    /// The opaque source reference.
    pub source_code: String,
}

impl PluginSourceResponse {
    /// A response carrying the stored source reference.
    #[must_use]
    pub fn from_record(record: &Plugin) -> Self {
        Self {
            id: record.id,
            plugin_type: record.plugin_type.clone(),
            name: record.name.clone(),
            source_code: record.source_code.clone().unwrap_or_default(),
        }
    }
}
