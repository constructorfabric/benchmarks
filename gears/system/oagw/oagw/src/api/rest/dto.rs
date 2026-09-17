//! REST DTOs for the OAGW Control-Plane Management API (feature
//! `cpt-cf-oagw-feature-control-plane-api`, interface
//! `cpt-cf-oagw-interface-management-api`).
//!
//! The DTO tree mirrors the upstream/route JSON schemas (DoD
//! `cpt-cf-oagw-dod-control-plane-api-upstream-crud`,
//! `cpt-cf-oagw-dod-control-plane-api-route-crud`,
//! `cpt-cf-oagw-dod-control-plane-api-plugin-crud`): request bodies carry the
//! optional-with-schema-defaults shape of the schemas (`server`/`protocol` /
//! `upstream_id`/`match` required; everything else optional and defaulted),
//! responses carry the full server-generated representation.  The
//! `api_dto` macro applies `snake_case` wire naming, which matches the
//! schemas; field-level `#[serde(rename)]` overrides cover the schema's
//! `type` (auth) and `match` (route) property names.
//!
//! Management DTO validation (algorithm
//! `cpt-cf-oagw-algo-control-plane-api-validate-dto`): endpoint format, pool
//! uniformity, `cred://` reference validity, and the CORS
//! `allow_credentials` + wildcard-origin conflict are validated here and
//! surface as `DomainError::validation` (400, GTS instance
//! `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value as JsonValue;
use uuid::Uuid;

use crate::domain::entity::alias::{standard_port, validate_rfc1123_hostname};
use crate::domain::entity::config::{
    BurstConfig, CorsConfig, EndpointScheme, HeadersConfig, PassthroughMode, PluginsConfig,
    RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow,
    RequestHeadersConfig, ResponseHeadersConfig, SharingMode, SustainedRate, UpstreamProtocol,
};
use crate::domain::entity::plugin::{Plugin, PluginType};
use crate::domain::entity::route::{
    GrpcMatch, HttpMatch, PathSuffixMode, Route, RouteMatch, RouteMethod,
};
use crate::domain::entity::upstream::{AuthConfig, Endpoint, Upstream};
use crate::domain::entity::{SecretRef, ServerConfig};
use crate::domain::error::DomainError;
use crate::domain::service::control_plane::{RouteDraft, UpstreamDraft};

// ---------------------------------------------------------------------------
// Shared value enums (schema enums)
// ---------------------------------------------------------------------------

/// Endpoint scheme of a server-pool member (schema `scheme` enum:
/// `https|http|wss|wt|grpc`).  Plaintext `http` is part of the management
/// contract so the SSRF guard's configuration-boundary decision is
/// observable on the wire (FEATURE `cpt-cf-oagw-dod-domain-model-repositories-ssrf`,
/// scenario `inst-dm-ssrf-scheme`); a plaintext pool is accepted only when
/// the gear's `allow_http_upstream` is enabled and rejected otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum EndpointSchemeDto {
    Https,
    Http,
    Wss,
    Wt,
    Grpc,
}

/// Hierarchical-config sharing mode (`private` | `inherit` | `enforce`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[schema(as = HierarchicalSharingModeDto)] // distinct name: `SharingModeDto` is owned by the credstore gear in the shared OpenAPI registry
pub enum SharingModeDto {
    Private,
    Inherit,
    Enforce,
}

/// Rate limiting algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitAlgorithmDto {
    TokenBucket,
    SlidingWindow,
}

/// Time window for a sustained rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitWindowDto {
    Second,
    Minute,
    Hour,
    Day,
}

/// Scope for rate-limit counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitScopeDto {
    Global,
    Tenant,
    User,
    Ip,
    Route,
}

/// Behavior when the limit is exceeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum RateLimitStrategyDto {
    Reject,
    Queue,
    Degrade,
}

/// Inbound header forwarding mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum PassthroughModeDto {
    None,
    Allowlist,
    All,
}

/// HTTP method allowlist member (schema `methods` enum, uppercase wire
/// tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum RouteMethodDto {
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

/// How the `/path_suffix` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum PathSuffixModeDto {
    Disabled,
    Append,
}

/// Custom plugin type (`auth` | `guard` | `transform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum PluginTypeDto {
    Auth,
    Guard,
    Transform,
}

// ---------------------------------------------------------------------------
// Shared nested value objects (schema `definitions`)
// ---------------------------------------------------------------------------

/// One upstream server-pool member.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct EndpointDto {
    pub scheme: EndpointSchemeDto,
    /// Hostname or IP address of the upstream service.
    pub host: String,
    /// Optional; defaults to the scheme's standard port (HTTP:80, TLS:443).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl EndpointDto {
    /// Validates the endpoint shape (algorithm
    /// `cpt-cf-oagw-algo-control-plane-api-validate-dto`, step
    /// `inst-cp-dto-endpoint`): the host must be RFC 1123 hostname-compliant
    /// and the port (when given) in `1..=65535`; the scheme is type-restricted
    /// by the DTO enum.
    fn validate(&self) -> Result<(), DomainError> {
        validate_rfc1123_hostname(&self.host).map_err(|e| {
            DomainError::validation(
                Some("server.endpoints"),
                format!("endpoint host invalid: {e}"),
            )
        })?;
        if let Some(port) = self.port
            && port == 0
        {
            return Err(DomainError::validation(
                Some("server.endpoints"),
                "endpoint port must be in 1..=65535".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Upstream server pool — one or more uniform endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct ServerDto {
    /// Non-empty per the schema `minItems: 1`.
    pub endpoints: Vec<EndpointDto>,
}

/// Authentication plugin configuration for an upstream (schema `auth`).
#[derive(Debug, Clone, Default, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct AuthDto {
    /// GTS identifier of the auth plugin type (schema property `type`), e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingModeDto>,
    /// Auth plugin configuration; secret material is referenced by `cred://`
    /// URIs resolved through the CredStore at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<JsonValue>,
}

/// Plugin chain configuration (schema `plugins`) — items are plugin
/// references: builtin plugins by GTS identifier, custom plugins by UUID.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct PluginsDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingModeDto>,
    /// Builtin plugins referenced by GTS ID, custom plugins by UUID.
    #[serde(default)]
    pub items: Vec<String>,
}

/// Sustained rate: tokens replenished per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct SustainedRateDto {
    pub rate: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<RateLimitWindowDto>,
}

/// Burst capacity for the token bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct BurstDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<u64>,
}

/// Rate limiting configuration (shared upstream/route schema definition).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RateLimitDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingModeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<RateLimitAlgorithmDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sustained: Option<SustainedRateDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<RateLimitScopeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<RateLimitStrategyDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<u64>,
}

/// CORS configuration (shared upstream/route schema definition).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct CorsDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharing: Option<SharingModeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_methods: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_headers: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_credentials: Option<bool>,
}

/// Header transformation rules for inbound requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RequestHeadersDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough: Option<PassthroughModeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// Header transformation rules for outbound responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeadersDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
}

/// Header transformation rules for requests/responses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct HeadersDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeadersDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeadersDto>,
}

// ---------------------------------------------------------------------------
// Route match objects (schema `match` — exactly one of http|grpc)
// ---------------------------------------------------------------------------

/// HTTP match rules (used when the upstream protocol is HTTP).  The schema
/// property is `path` (the derived route's `path_prefix`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct HttpMatchDto {
    /// HTTP methods supported by this route (non-empty).
    pub methods: Vec<RouteMethodDto>,
    /// Path pattern for the route.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_allowlist: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_suffix_mode: Option<PathSuffixModeDto>,
}

/// gRPC match rules — present in the contract but reserved (Phase 3); the
/// service rejects gRPC match creation (DoD
/// `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct MatchGrpcDto {
    /// Fully qualified gRPC service name (e.g. `foo.v1.UserService`).
    pub service: String,
    /// RPC method name (e.g. `GetUser`).
    pub method: String,
}

/// Protocol-scoped inbound match rules — externally tagged so the wire shape
/// is exactly `{"http": {...}}` or `{"grpc": {...}}` per the schema's
/// `oneOf`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum MatchDto {
    Http(HttpMatchDto),
    Grpc(MatchGrpcDto),
}

// ---------------------------------------------------------------------------
// Upstream request / response
// ---------------------------------------------------------------------------

/// Request body for `POST /api/oagw/v1/upstreams` (and `PUT` replace).
///
/// `server` and `protocol` are required; every other field is optional and
/// defaulted per the upstream schema.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequestDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Human-readable routing identifier; auto-derived for hostname pools
    /// (a user-provided alias matching the derived value is an idempotent
    /// no-op; a non-matching alias on a hostname pool is rejected).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    pub server: ServerDto,
    /// Protocol GTS identifier
    /// (`gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1` |
    /// `...grpc.v1`).
    pub protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

/// Full upstream representation returned by create/list/get (schema shape
/// with the server-generated `id`).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamViewDto {
    pub id: Uuid,
    #[serde(default)]
    pub enabled: bool,
    pub alias: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: ServerDto,
    pub protocol: String,
    pub auth: AuthDto,
    pub headers: HeadersDto,
    #[serde(default)]
    pub plugins: PluginsDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

// ---------------------------------------------------------------------------
// Route request / response
// ---------------------------------------------------------------------------

/// Request body for `POST /api/oagw/v1/routes` (and `PUT` replace).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequestDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Reference to the upstream for this route (immutable once set).
    pub upstream_id: Uuid,
    /// Protocol-scoped inbound matching rules (exactly one of http|grpc).
    #[serde(rename = "match")]
    pub match_: MatchDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
    /// Route priority for tie-breaking (default 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Full route representation returned by create/list/get.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteViewDto {
    pub id: Uuid,
    #[serde(default)]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_: MatchDto,
    #[serde(default)]
    pub plugins: PluginsDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Plugin request / response
// ---------------------------------------------------------------------------

/// Request body for `POST /api/oagw/v1/plugins`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginCreateDto {
    pub plugin_type: PluginTypeDto,
    /// Unique plugin name per tenant `(tenant_id, name)`.
    pub name: String,
    /// JSON schema for plugin configuration.
    pub config_schema: JsonValue,
    /// Starlark source.
    pub source_code: String,
}

/// Full custom-plugin representation returned by create/list/get.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginViewDto {
    pub id: Uuid,
    pub plugin_type: PluginTypeDto,
    pub name: String,
    pub config_schema: JsonValue,
    /// Starlark source (immutable after creation).
    pub source_code: String,
}

/// Body for `GET /api/oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// The plugin's Starlark `source_code`.
    pub source_code: String,
}

// ---------------------------------------------------------------------------
// List query parameters (OData-style)
// ---------------------------------------------------------------------------

/// Per-operation list query parameters (`$top`, `$skip`, `$filter`,
/// `$select`, `$orderby`), algorithm
/// `cpt-cf-oagw-algo-control-plane-api-odata`.
///
/// `$top` defaults to 50 and is capped at 100 (`inst-cp-odata-paging`);
/// `$filter` is equality-only (unsupported expressions are a 400);
/// `$orderby` supports `field [asc|desc]` on the documented keys.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListParams {
    #[serde(rename = "$top", default)]
    pub top: Option<u32>,
    #[serde(rename = "$skip", default)]
    pub skip: Option<u32>,
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
    #[serde(rename = "$select", default)]
    #[allow(dead_code)]
    // accepted for schema compatibility; list views are full (projection is a no-op)
    pub select: Option<String>,
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,
}

impl ListParams {
    /// Default page size (`inst-cp-odata-paging`).
    pub const DEFAULT_TOP: u32 = 50;
    /// Hard cap for `$top` (`inst-cp-odata-paging`).
    pub const MAX_TOP: u32 = 100;

    /// The effective page size: `$top` (capped at [`Self::MAX_TOP`]) or
    /// [`Self::DEFAULT_TOP`].
    #[must_use]
    pub fn top(&self) -> usize {
        self.top
            .unwrap_or(Self::DEFAULT_TOP)
            .clamp(0, Self::MAX_TOP) as usize
    }

    /// The effective `$skip` (default 0).
    #[must_use]
    pub fn skip(&self) -> usize {
        self.skip.unwrap_or(0) as usize
    }
}

/// A parsed equality `$filter` predicate (`field eq value` | `field ne
/// value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EqFilter {
    pub field: String,
    /// `true` for `eq`, `false` for `ne`.
    pub is_eq: bool,
    pub value: String,
}

impl EqFilter {
    /// Parses an equality-only `$filter` expression of the form
    /// `field eq 'value'` / `field ne 'value'` (single-quoted or bare
    /// tokens).  Anything else is a 400 validation error (the list contract
    /// is equality-only; unsupported operators fail explicitly rather than
    /// being silently ignored).
    ///
    /// # Errors
    /// [`DomainError::Validation`] when the expression is not a
    /// single equality predicate.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        match tokens.as_slice() {
            [field, op, value] if *op == "eq" || *op == "ne" => {
                if field.is_empty() {
                    return Err(DomainError::validation(
                        Some("$filter"),
                        format!("unsupported $filter expression: '{raw}'"),
                    ));
                }
                let value = value.trim_matches('\'').to_owned();
                Ok(Self {
                    field: (*field).to_owned(),
                    is_eq: *op == "eq",
                    value,
                })
            }
            _ => Err(DomainError::validation(
                Some("$filter"),
                format!(
                    "unsupported $filter expression (equality only): '{raw}'; \
                     expected '<field> eq|ne <value>'"
                ),
            )),
        }
    }
}

/// A parsed `$orderby` directive (`field [asc|desc]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub field: String,
    pub desc: bool,
}

impl OrderBy {
    /// Parses `$orderby=field [asc|desc]` (default `asc`).
    ///
    /// # Errors
    /// [`DomainError::Validation`] when the expression is not a single
    /// sort directive.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        match tokens.as_slice() {
            [field] => Ok(Self {
                field: (*field).to_owned(),
                desc: false,
            }),
            [field, dir] if *dir == "asc" || *dir == "desc" => Ok(Self {
                field: (*field).to_owned(),
                desc: *dir == "desc",
            }),
            _ => Err(DomainError::validation(
                Some("$orderby"),
                format!("unsupported $orderby expression: '{raw}'; expected '<field> [asc|desc]'"),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// DTO ⟷ domain conversions (algorithm
// `cpt-cf-oagw-algo-control-plane-api-validate-dto`; the DTO layer converts
// request DTOs into domain drafts so the domain never depends on API types,
// and entities into response DTOs).
// ---------------------------------------------------------------------------

/// Scans a JSON value for `cred://` reference strings and validates each one
/// (malformed references surface as a 400 rather than reaching the domain).
fn validate_cred_refs(value: &JsonValue) -> Result<(), DomainError> {
    match value {
        JsonValue::String(s) if s.starts_with(SecretRef::PREFIX) => SecretRef::try_new(s.clone())
            .map(|_| ())
            .map_err(|e| DomainError::validation(Some("auth.config"), e)),
        JsonValue::Object(map) => {
            for v in map.values() {
                validate_cred_refs(v)?;
            }
            Ok(())
        }
        JsonValue::Array(items) => {
            for item in items {
                validate_cred_refs(item)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// --- value enums: DTO → entity ----------------------------------------------

macro_rules! dto_to_entity_enum {
    ($dto:ty, $entity:ty, { $($v:ident => $e:expr),* $(,)? }) => {
        impl $dto {
            /// Converts into the domain enum.
            #[must_use]
            pub fn into_entity(self) -> $entity {
                match self {
                    $( Self::$v => $e, )*
                }
            }
        }
    };
}

dto_to_entity_enum!(EndpointSchemeDto, EndpointScheme, {
    Https => EndpointScheme::Https,
    Http => EndpointScheme::Http,
    Wss => EndpointScheme::Wss,
    Wt => EndpointScheme::Wt,
    Grpc => EndpointScheme::Grpc,
});
dto_to_entity_enum!(SharingModeDto, SharingMode, {
    Private => SharingMode::Private,
    Inherit => SharingMode::Inherit,
    Enforce => SharingMode::Enforce,
});
dto_to_entity_enum!(RateLimitAlgorithmDto, RateLimitAlgorithm, {
    TokenBucket => RateLimitAlgorithm::TokenBucket,
    SlidingWindow => RateLimitAlgorithm::SlidingWindow,
});
dto_to_entity_enum!(RateLimitWindowDto, RateLimitWindow, {
    Second => RateLimitWindow::Second,
    Minute => RateLimitWindow::Minute,
    Hour => RateLimitWindow::Hour,
    Day => RateLimitWindow::Day,
});
dto_to_entity_enum!(RateLimitScopeDto, RateLimitScope, {
    Global => RateLimitScope::Global,
    Tenant => RateLimitScope::Tenant,
    User => RateLimitScope::User,
    Ip => RateLimitScope::Ip,
    Route => RateLimitScope::Route,
});
dto_to_entity_enum!(RateLimitStrategyDto, RateLimitStrategy, {
    Reject => RateLimitStrategy::Reject,
    Queue => RateLimitStrategy::Queue,
    Degrade => RateLimitStrategy::Degrade,
});
dto_to_entity_enum!(PassthroughModeDto, PassthroughMode, {
    None => PassthroughMode::None,
    Allowlist => PassthroughMode::Allowlist,
    All => PassthroughMode::All,
});
dto_to_entity_enum!(RouteMethodDto, RouteMethod, {
    Get => RouteMethod::Get,
    Post => RouteMethod::Post,
    Put => RouteMethod::Put,
    Delete => RouteMethod::Delete,
    Patch => RouteMethod::Patch,
});
dto_to_entity_enum!(PathSuffixModeDto, PathSuffixMode, {
    Disabled => PathSuffixMode::Disabled,
    Append => PathSuffixMode::Append,
});
dto_to_entity_enum!(PluginTypeDto, PluginType, {
    Auth => PluginType::Auth,
    Guard => PluginType::Guard,
    Transform => PluginType::Transform,
});

// --- value enums: entity → DTO ------------------------------------------------

macro_rules! entity_to_dto_enum {
    ($entity:ty, $dto:ty, { $($v:ident => $e:ident),* $(,)? }) => {
        impl From<$entity> for $dto {
            fn from(value: $entity) -> Self {
                match value {
                    $( <$entity>::$v => Self::$e, )*
                }
            }
        }
    };
}

entity_to_dto_enum!(EndpointScheme, EndpointSchemeDto, {
    Https => Https,
    Http => Http, // plaintext round-trips; the SSRF guard enforced it at persistence
    Wss => Wss,
    Wt => Wt,
    Grpc => Grpc,
});
entity_to_dto_enum!(SharingMode, SharingModeDto, {
    Private => Private,
    Inherit => Inherit,
    Enforce => Enforce,
});
entity_to_dto_enum!(RateLimitAlgorithm, RateLimitAlgorithmDto, {
    TokenBucket => TokenBucket,
    SlidingWindow => SlidingWindow,
});
entity_to_dto_enum!(RateLimitWindow, RateLimitWindowDto, {
    Second => Second,
    Minute => Minute,
    Hour => Hour,
    Day => Day,
});
entity_to_dto_enum!(RateLimitScope, RateLimitScopeDto, {
    Global => Global,
    Tenant => Tenant,
    User => User,
    Ip => Ip,
    Route => Route,
});
entity_to_dto_enum!(RateLimitStrategy, RateLimitStrategyDto, {
    Reject => Reject,
    Queue => Queue,
    Degrade => Degrade,
});
entity_to_dto_enum!(PassthroughMode, PassthroughModeDto, {
    None => None,
    Allowlist => Allowlist,
    All => All,
});
entity_to_dto_enum!(RouteMethod, RouteMethodDto, {
    Get => Get,
    Post => Post,
    Put => Put,
    Delete => Delete,
    Patch => Patch,
});
entity_to_dto_enum!(PathSuffixMode, PathSuffixModeDto, {
    Disabled => Disabled,
    Append => Append,
});
entity_to_dto_enum!(PluginType, PluginTypeDto, {
    Auth => Auth,
    Guard => Guard,
    Transform => Transform,
});

// --- nested value objects: DTO → entity --------------------------------------

impl EndpointDto {
    /// Converts into the domain endpoint, applying the scheme's standard port
    /// when no explicit port was given.
    #[must_use]
    pub fn into_entity(self) -> Endpoint {
        let scheme = self.scheme.into_entity();
        Endpoint {
            scheme,
            host: self.host,
            port: self
                .port
                .unwrap_or_else(|| standard_port(scheme).unwrap_or(443)),
        }
    }
}

impl From<&Endpoint> for EndpointDto {
    fn from(ep: &Endpoint) -> Self {
        Self {
            scheme: EndpointSchemeDto::from(ep.scheme),
            host: ep.host.clone(),
            port: Some(ep.port),
        }
    }
}

impl ServerDto {
    /// Converts into the domain server pool after validating the pool
    /// uniformity invariant (`minItems: 1`, uniform scheme, uniform port).
    ///
    /// # Errors
    /// [`DomainError::Validation`] on an empty pool or a non-uniform pool.
    pub fn into_entity(self) -> Result<ServerConfig, DomainError> {
        if self.endpoints.is_empty() {
            return Err(DomainError::validation(
                Some("server.endpoints"),
                "at least one endpoint is required",
            ));
        }
        let first = self.endpoints[0].clone();
        let first_scheme = first.scheme;
        let first_port = first
            .port
            .unwrap_or_else(|| standard_port(first_scheme.into_entity()).unwrap_or(443));
        let mut endpoints = Vec::with_capacity(self.endpoints.len());
        for ep in self.endpoints {
            ep.validate()?;
            if ep.scheme != first_scheme {
                return Err(DomainError::validation(
                    Some("server.endpoints"),
                    "all endpoints in a pool must share the same scheme",
                ));
            }
            let effective_port = ep
                .port
                .unwrap_or_else(|| standard_port(ep.scheme.into_entity()).unwrap_or(443));
            if effective_port != first_port {
                return Err(DomainError::validation(
                    Some("server.endpoints"),
                    "all endpoints in a pool must share the same port",
                ));
            }
            endpoints.push(ep.into_entity());
        }
        Ok(ServerConfig { endpoints })
    }
}

impl From<&ServerConfig> for ServerDto {
    fn from(server: &ServerConfig) -> Self {
        Self {
            endpoints: server.endpoints.iter().map(Into::into).collect(),
        }
    }
}

impl AuthDto {
    /// Converts into the domain auth configuration (defaults: no plugin type,
    /// `private` sharing, `null` config).
    #[must_use]
    pub fn into_entity(self) -> AuthConfig {
        AuthConfig {
            plugin_type: self.auth_type,
            sharing: self
                .sharing
                .unwrap_or(SharingModeDto::Private)
                .into_entity(),
            config: self.config.unwrap_or(JsonValue::Null),
        }
    }
}

impl From<&AuthConfig> for AuthDto {
    fn from(auth: &AuthConfig) -> Self {
        Self {
            auth_type: auth.plugin_type.clone(),
            sharing: Some(SharingModeDto::from(auth.sharing)),
            config: Some(auth.config.clone()),
        }
    }
}

impl PluginsDto {
    /// Splits into `(plugin_refs, sharing mode)` for the domain draft.
    #[must_use]
    pub fn into_parts(self) -> (Vec<String>, SharingMode) {
        let sharing = self
            .sharing
            .unwrap_or(SharingModeDto::Private)
            .into_entity();
        (self.items, sharing)
    }
}

impl From<&PluginsConfig> for PluginsDto {
    fn from(plugins: &PluginsConfig) -> Self {
        Self {
            sharing: Some(SharingModeDto::from(plugins.sharing)),
            items: plugins.items.iter().map(|b| b.plugin_ref.clone()).collect(),
        }
    }
}

impl SustainedRateDto {
    #[must_use]
    pub fn into_entity(self) -> SustainedRate {
        SustainedRate {
            rate: self.rate,
            window: self
                .window
                .unwrap_or(RateLimitWindowDto::Second)
                .into_entity(),
        }
    }
}

impl BurstDto {
    #[must_use]
    pub fn into_entity(self) -> BurstConfig {
        BurstConfig {
            capacity: self.capacity,
        }
    }
}

impl RateLimitDto {
    /// Converts into the domain rate-limit config, layering the provided
    /// values over the schema defaults.
    #[must_use]
    pub fn into_entity(self) -> RateLimitConfig {
        let base = RateLimitConfig::default();
        let sustained = self.sustained.map(SustainedRateDto::into_entity);
        RateLimitConfig {
            sharing: self
                .sharing
                .unwrap_or(SharingModeDto::from(base.sharing))
                .into_entity(),
            algorithm: self
                .algorithm
                .unwrap_or(RateLimitAlgorithmDto::from(base.algorithm))
                .into_entity(),
            sustained: SustainedRate {
                rate: sustained.as_ref().map_or(base.sustained.rate, |s| s.rate),
                window: sustained.map_or(base.sustained.window, |s| s.window),
            },
            burst: self.burst.map(BurstDto::into_entity),
            scope: self
                .scope
                .unwrap_or(RateLimitScopeDto::from(base.scope))
                .into_entity(),
            strategy: self
                .strategy
                .unwrap_or(RateLimitStrategyDto::from(base.strategy))
                .into_entity(),
            cost: self.cost.unwrap_or(base.cost),
        }
    }
}

impl From<&RateLimitConfig> for RateLimitDto {
    fn from(cfg: &RateLimitConfig) -> Self {
        Self {
            sharing: Some(SharingModeDto::from(cfg.sharing)),
            algorithm: Some(RateLimitAlgorithmDto::from(cfg.algorithm)),
            sustained: Some(SustainedRateDto {
                rate: cfg.sustained.rate,
                window: Some(RateLimitWindowDto::from(cfg.sustained.window)),
            }),
            burst: cfg.burst.map(|b| BurstDto {
                capacity: b.capacity,
            }),
            scope: Some(RateLimitScopeDto::from(cfg.scope)),
            strategy: Some(RateLimitStrategyDto::from(cfg.strategy)),
            cost: Some(cfg.cost),
        }
    }
}

impl CorsDto {
    /// Converts into the domain CORS config, layering the provided values
    /// over the schema defaults.
    #[must_use]
    pub fn into_entity(self) -> CorsConfig {
        let base = CorsConfig::default();
        CorsConfig {
            sharing: self
                .sharing
                .unwrap_or(SharingModeDto::from(base.sharing))
                .into_entity(),
            enabled: self.enabled.unwrap_or(base.enabled),
            allowed_origins: self
                .allowed_origins
                .unwrap_or_else(|| base.allowed_origins.clone()),
            allowed_methods: self
                .allowed_methods
                .unwrap_or_else(|| base.allowed_methods.clone()),
            expose_headers: self
                .expose_headers
                .unwrap_or_else(|| base.expose_headers.clone()),
            allow_credentials: self.allow_credentials.unwrap_or(base.allow_credentials),
        }
    }
}

impl From<&CorsConfig> for CorsDto {
    fn from(cors: &CorsConfig) -> Self {
        Self {
            sharing: Some(SharingModeDto::from(cors.sharing)),
            enabled: Some(cors.enabled),
            allowed_origins: Some(cors.allowed_origins.clone()),
            allowed_methods: Some(cors.allowed_methods.clone()),
            expose_headers: Some(cors.expose_headers.clone()),
            allow_credentials: Some(cors.allow_credentials),
        }
    }
}

impl RequestHeadersDto {
    #[must_use]
    pub fn into_entity(self) -> RequestHeadersConfig {
        RequestHeadersConfig {
            set: self.set.unwrap_or_default(),
            add: self.add.unwrap_or_default(),
            remove: self.remove.unwrap_or_default(),
            passthrough: self
                .passthrough
                .unwrap_or(PassthroughModeDto::None)
                .into_entity(),
            passthrough_allowlist: self.passthrough_allowlist.unwrap_or_default(),
        }
    }
}

impl From<&RequestHeadersConfig> for RequestHeadersDto {
    fn from(cfg: &RequestHeadersConfig) -> Self {
        Self {
            set: Some(cfg.set.clone()),
            add: Some(cfg.add.clone()),
            remove: Some(cfg.remove.clone()),
            passthrough: Some(PassthroughModeDto::from(cfg.passthrough)),
            passthrough_allowlist: Some(cfg.passthrough_allowlist.clone()),
        }
    }
}

impl ResponseHeadersDto {
    #[must_use]
    pub fn into_entity(self) -> ResponseHeadersConfig {
        ResponseHeadersConfig {
            set: self.set.unwrap_or_default(),
            add: self.add.unwrap_or_default(),
            remove: self.remove.unwrap_or_default(),
        }
    }
}

impl From<&ResponseHeadersConfig> for ResponseHeadersDto {
    fn from(cfg: &ResponseHeadersConfig) -> Self {
        Self {
            set: Some(cfg.set.clone()),
            add: Some(cfg.add.clone()),
            remove: Some(cfg.remove.clone()),
        }
    }
}

impl HeadersDto {
    #[must_use]
    pub fn into_entity(self) -> HeadersConfig {
        HeadersConfig {
            request: self
                .request
                .map(RequestHeadersDto::into_entity)
                .unwrap_or_default(),
            response: self
                .response
                .map(ResponseHeadersDto::into_entity)
                .unwrap_or_default(),
        }
    }
}

impl From<&HeadersConfig> for HeadersDto {
    fn from(cfg: &HeadersConfig) -> Self {
        Self {
            request: Some(RequestHeadersDto::from(&cfg.request)),
            response: Some(ResponseHeadersDto::from(&cfg.response)),
        }
    }
}

// --- route match objects: DTO → entity ----------------------------------------

impl HttpMatchDto {
    /// Converts into the domain HTTP match after validating the schema
    /// `minItems`/`minLength` invariants.
    ///
    /// # Errors
    /// [`DomainError::Validation`] on an empty method list or empty path.
    pub fn into_entity(self) -> Result<HttpMatch, DomainError> {
        if self.methods.is_empty() {
            return Err(DomainError::validation(
                Some("match.methods"),
                "at least one HTTP method is required",
            ));
        }
        if self.path.trim().is_empty() {
            return Err(DomainError::validation(
                Some("match.path"),
                "an HTTP route requires a non-empty path prefix",
            ));
        }
        Ok(HttpMatch {
            methods: self
                .methods
                .into_iter()
                .map(RouteMethodDto::into_entity)
                .collect(),
            path_prefix: self.path,
            query_allowlist: self.query_allowlist.unwrap_or_default(),
            path_suffix_mode: self
                .path_suffix_mode
                .unwrap_or(PathSuffixModeDto::Append)
                .into_entity(),
        })
    }
}

impl From<&HttpMatch> for HttpMatchDto {
    fn from(m: &HttpMatch) -> Self {
        Self {
            methods: m.methods.iter().copied().map(Into::into).collect(),
            path: m.path_prefix.clone(),
            query_allowlist: Some(m.query_allowlist.clone()),
            path_suffix_mode: Some(PathSuffixModeDto::from(m.path_suffix_mode)),
        }
    }
}

impl MatchGrpcDto {
    #[must_use]
    pub fn into_entity(self) -> GrpcMatch {
        GrpcMatch {
            service: self.service,
            method: self.method,
        }
    }
}

impl From<&GrpcMatch> for MatchGrpcDto {
    fn from(m: &GrpcMatch) -> Self {
        Self {
            service: m.service.clone(),
            method: m.method.clone(),
        }
    }
}

impl MatchDto {
    /// Converts into the domain route-match value object (rejecting nothing
    /// here — gRPC reservation is enforced at the service boundary).
    ///
    /// # Errors
    /// [`DomainError::Validation`] on a malformed HTTP match.
    pub fn into_entity(self) -> Result<RouteMatch, DomainError> {
        match self {
            Self::Http(http) => Ok(RouteMatch::Http(http.into_entity()?)),
            Self::Grpc(grpc) => Ok(RouteMatch::Grpc(grpc.into_entity())),
        }
    }
}

impl From<&RouteMatch> for MatchDto {
    fn from(m: &RouteMatch) -> Self {
        match m {
            RouteMatch::Http(http) => Self::Http(HttpMatchDto::from(http)),
            RouteMatch::Grpc(grpc) => Self::Grpc(MatchGrpcDto::from(grpc)),
        }
    }
}

// --- request DTOs → domain drafts ---------------------------------------------

impl UpstreamRequestDto {
    /// Validates the full upstream request (algorithm
    /// `cpt-cf-oagw-algo-control-plane-api-validate-dto`): endpoint format and
    /// pool uniformity, the CORS `allow_credentials` + wildcard-origin
    /// conflict, and `cred://` reference validity.
    ///
    /// # Errors
    /// [`DomainError::Validation`] (400) on any violation.
    pub fn validate(&self) -> Result<(), DomainError> {
        // Pool shape: minItems 1, uniform scheme, uniform effective port.
        if self.server.endpoints.is_empty() {
            return Err(DomainError::validation(
                Some("server.endpoints"),
                "at least one endpoint is required",
            ));
        }
        let first = &self.server.endpoints[0];
        for ep in &self.server.endpoints {
            ep.validate()?;
            if ep.scheme != first.scheme {
                return Err(DomainError::validation(
                    Some("server.endpoints"),
                    "all endpoints in a pool must share the same scheme",
                ));
            }
        }
        let first_port = first
            .port
            .unwrap_or_else(|| standard_port(first.scheme.into_entity()).unwrap_or(443));
        for ep in &self.server.endpoints {
            let effective_port = ep
                .port
                .unwrap_or_else(|| standard_port(ep.scheme.into_entity()).unwrap_or(443));
            if effective_port != first_port {
                return Err(DomainError::validation(
                    Some("server.endpoints"),
                    "all endpoints in a pool must share the same port",
                ));
            }
        }

        // CORS: allow_credentials + wildcard origin are mutually exclusive
        // (browsers refuse credentialed requests to `*`).
        if let Some(cors) = &self.cors
            && cors.allow_credentials == Some(true)
            && cors
                .allowed_origins
                .as_deref()
                .is_some_and(|origins| origins.iter().any(|o| o == "*"))
        {
            return Err(DomainError::validation(
                Some("cors.allowed_origins"),
                "CORS allow_credentials cannot be combined with the wildcard origin '*'",
            ));
        }

        // `cred://` secret references must be well-formed.
        if let Some(auth) = &self.auth
            && let Some(config) = &auth.config
        {
            validate_cred_refs(config)?;
        }
        Ok(())
    }

    /// Converts this request into a domain [`UpstreamDraft`], validating it
    /// first.
    ///
    /// # Errors
    /// [`DomainError::Validation`] (400) on any violation.
    pub fn into_draft(self) -> Result<UpstreamDraft, DomainError> {
        self.validate()?;
        let protocol: UpstreamProtocol = self
            .protocol
            .parse()
            .map_err(|e| DomainError::validation(Some("protocol"), e))?;
        let server = self.server.into_entity()?;
        let (plugin_refs, plugins_sharing) =
            self.plugins.map(PluginsDto::into_parts).unwrap_or_default();
        Ok(UpstreamDraft {
            alias: self.alias,
            enabled: self.enabled,
            tags: self.tags.unwrap_or_default(),
            server,
            protocol,
            auth: self.auth.map(AuthDto::into_entity).unwrap_or_default(),
            headers: self
                .headers
                .map(HeadersDto::into_entity)
                .unwrap_or_default(),
            rate_limit: self.rate_limit.map(RateLimitDto::into_entity),
            cors: self.cors.map(CorsDto::into_entity),
            plugin_refs,
            plugins_sharing,
        })
    }
}

impl RouteRequestDto {
    /// Validates the route request: the CORS `allow_credentials` +
    /// wildcard-origin conflict and the match shape (non-empty methods and
    /// path).
    ///
    /// # Errors
    /// [`DomainError::Validation`] (400) on any violation.
    pub fn validate(&self) -> Result<(), DomainError> {
        if let Some(cors) = &self.cors
            && cors.allow_credentials == Some(true)
            && cors
                .allowed_origins
                .as_deref()
                .is_some_and(|origins| origins.iter().any(|o| o == "*"))
        {
            return Err(DomainError::validation(
                Some("cors.allowed_origins"),
                "CORS allow_credentials cannot be combined with the wildcard origin '*'",
            ));
        }
        match &self.match_ {
            MatchDto::Http(http) => {
                if http.methods.is_empty() {
                    return Err(DomainError::validation(
                        Some("match.methods"),
                        "at least one HTTP method is required",
                    ));
                }
                if http.path.trim().is_empty() {
                    return Err(DomainError::validation(
                        Some("match.path"),
                        "an HTTP route requires a non-empty path prefix",
                    ));
                }
            }
            MatchDto::Grpc(_) => {}
        }
        Ok(())
    }

    /// Converts this request into a domain [`RouteDraft`], validating it
    /// first.
    ///
    /// # Errors
    /// [`DomainError::Validation`] (400) on any violation.
    pub fn into_draft(self) -> Result<RouteDraft, DomainError> {
        self.validate()?;
        let match_ = self.match_.into_entity()?;
        let (plugin_refs, plugins_sharing) =
            self.plugins.map(PluginsDto::into_parts).unwrap_or_default();
        Ok(RouteDraft {
            upstream_id: self.upstream_id,
            match_,
            priority: self.priority.unwrap_or(0),
            enabled: self.enabled.unwrap_or(true),
            tags: self.tags.unwrap_or_default(),
            rate_limit: self.rate_limit.map(RateLimitDto::into_entity),
            cors: self.cors.map(CorsDto::into_entity),
            plugin_refs,
            plugins_sharing,
        })
    }
}

// --- entities → response DTOs -------------------------------------------------

impl From<&Upstream> for UpstreamViewDto {
    fn from(u: &Upstream) -> Self {
        Self {
            id: u.id,
            enabled: u.enabled,
            alias: u.alias.clone(),
            tags: u.tags.clone(),
            server: ServerDto::from(&u.server),
            protocol: u.protocol.gts_id().to_owned(),
            auth: AuthDto::from(&u.auth),
            headers: HeadersDto::from(&u.headers),
            plugins: PluginsDto::from(&u.plugins),
            rate_limit: u.rate_limit.as_ref().map(RateLimitDto::from),
            cors: u.cors.as_ref().map(CorsDto::from),
        }
    }
}

impl From<&Route> for RouteViewDto {
    fn from(r: &Route) -> Self {
        Self {
            id: r.id,
            tags: r.tags.clone(),
            upstream_id: r.upstream_id,
            match_: MatchDto::from(&r.match_),
            plugins: PluginsDto::from(&r.plugins),
            rate_limit: r.rate_limit.as_ref().map(RateLimitDto::from),
            cors: r.cors.as_ref().map(CorsDto::from),
            priority: r.priority,
            enabled: r.enabled,
        }
    }
}

impl From<&Plugin> for PluginViewDto {
    fn from(p: &Plugin) -> Self {
        Self {
            id: p.id,
            plugin_type: PluginTypeDto::from(p.plugin_type),
            name: p.name.clone(),
            config_schema: p.config_schema.clone(),
            source_code: p.source_code.clone(),
        }
    }
}

/// Convenience constructor for the plugin-source body so handlers and tests
/// share a single construction path.
#[must_use]
pub fn plugin_source_dto(source: String) -> PluginSourceDto {
    PluginSourceDto {
        source_code: source,
    }
}
