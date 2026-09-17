//! REST DTOs of the management API
//! ([DESIGN.md](../../../docs/DESIGN.md) `cpt-cf-oagw-interface-api`).
//!
//! Every request field is `Option`, so *all* payload rules (required fields,
//! alias and tag patterns, endpoint shapes, `cors.allow_credentials` vs `*`)
//! are decided by the [`Service`](crate::domain::service::Service) and reported
//! as one canonical 400 with `field_violations` — never as an opaque
//! deserialization error. The configuration blocks (`server`, `auth`,
//! `headers`, `plugins`, `rate_limit`, `cors`, and a route's `match`) travel as
//! JSON values and are parsed into the phase-1 domain types by the service.
//!
//! Responses mirror the stored representation one-to-one; the `enabled` state
//! of a route is reported next to it because `route.v1.schema.json` has no
//! such property.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::model::{Route, Upstream};
use crate::domain::service::{ListParams, RouteInput, RouteRecord, UpstreamInput};

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Create/replace payload of an upstream (`POST`/`PUT`
/// `/oagw/v1/upstreams[/{id}]`).
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequestDto {
    /// Whether the upstream accepts proxy traffic; defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Explicit alias (`[a-z0-9]([a-z0-9.:-]*[a-z0-9])?`). Omitted, it is
    /// derived from the endpoint hostnames — and a differing value is rejected
    /// for hostname-based endpoints.
    #[serde(default)]
    pub alias: Option<String>,
    /// Categorization tags (`[a-z0-9_-]+`).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Endpoint pool: `{ "endpoints": [{ "scheme", "host", "port" }] }`.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub server: Option<Value>,
    /// Upstream protocol as its full GTS id.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Authentication plugin binding.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub auth: Option<Value>,
    /// Header transformation rules.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub headers: Option<Value>,
    /// Plugin chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: Option<Value>,
    /// Rate limiting configuration.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub rate_limit: Option<Value>,
    /// CORS configuration.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub cors: Option<Value>,
}

impl From<UpstreamRequestDto> for UpstreamInput {
    fn from(dto: UpstreamRequestDto) -> Self {
        Self {
            enabled: dto.enabled,
            alias: dto.alias,
            tags: dto.tags,
            server: dto.server,
            protocol: dto.protocol,
            auth: dto.auth,
            headers: dto.headers,
            plugins: dto.plugins,
            rate_limit: dto.rate_limit,
            cors: dto.cors,
        }
    }
}

/// Stored representation of an upstream.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Server-generated id.
    pub id: Uuid,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Routing key of `/oagw/v1/proxy/{alias}`.
    pub alias: String,
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    #[schema(value_type = Object)]
    pub server: Value,
    /// Upstream protocol as its full GTS id.
    pub protocol: String,
    /// Authentication plugin binding, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub auth: Option<Value>,
    /// Header transformation rules, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub headers: Option<Value>,
    /// Plugin chain, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<Value>,
    /// Rate limiting configuration, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<Value>,
    /// CORS configuration, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<Value>,
}

impl From<Upstream> for UpstreamDto {
    fn from(upstream: Upstream) -> Self {
        Self {
            id: upstream.id.unwrap_or_default(),
            enabled: upstream.enabled,
            alias: upstream
                .alias
                .map(|alias| alias.to_string())
                .unwrap_or_default(),
            tags: upstream.tags.iter().map(ToString::to_string).collect(),
            server: serde_json::to_value(upstream.server).unwrap_or_default(),
            protocol: upstream.protocol.gts_id().to_owned(),
            auth: upstream
                .auth
                .and_then(|block| serde_json::to_value(block).ok()),
            headers: upstream
                .headers
                .and_then(|block| serde_json::to_value(block).ok()),
            plugins: upstream
                .plugins
                .and_then(|block| serde_json::to_value(block).ok()),
            rate_limit: upstream
                .rate_limit
                .and_then(|block| serde_json::to_value(block).ok()),
            cors: upstream
                .cors
                .and_then(|block| serde_json::to_value(block).ok()),
        }
    }
}

/// `POST /oagw/v1/upstreams/{id}/status` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamStatusRequestDto {
    /// `false` makes the upstream resolve as unavailable on the proxy path.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Create/replace payload of a route (`POST`/`PUT` `/oagw/v1/routes[/{id}]`).
///
/// `upstream_id` is required on create and **immutable**: a `PUT` that names a
/// different upstream is a 400.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequestDto {
    /// Categorization tags (`[a-z0-9_-]+`).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Owning upstream of the calling tenant.
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    /// Protocol-scoped matching rules; `match` on the wire.
    #[serde(default, rename = "match")]
    #[schema(value_type = Object)]
    pub match_rule: Option<Value>,
    /// Plugin chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: Option<Value>,
    /// Rate limiting configuration.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub rate_limit: Option<Value>,
    /// CORS configuration.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub cors: Option<Value>,
}

impl From<RouteRequestDto> for RouteInput {
    fn from(dto: RouteRequestDto) -> Self {
        Self {
            tags: dto.tags,
            upstream_id: dto.upstream_id,
            match_rule: dto.match_rule,
            plugins: dto.plugins,
            rate_limit: dto.rate_limit,
            cors: dto.cors,
        }
    }
}

/// Stored representation of a route.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Server-generated id.
    pub id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Categorization tags.
    pub tags: Vec<String>,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Protocol-scoped matching rules.
    #[schema(value_type = Object)]
    pub match_rule: Value,
    /// Plugin chain, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub plugins: Option<Value>,
    /// Rate limiting configuration, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<Value>,
    /// CORS configuration, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<Value>,
}

impl From<Route> for RouteDto {
    fn from(route: Route) -> Self {
        Self {
            id: route.id.unwrap_or_default(),
            enabled: true,
            tags: route.tags.iter().map(ToString::to_string).collect(),
            upstream_id: route.upstream_id,
            match_rule: serde_json::to_value(route.match_rule).unwrap_or_default(),
            plugins: route
                .plugins
                .and_then(|block| serde_json::to_value(block).ok()),
            rate_limit: route
                .rate_limit
                .and_then(|block| serde_json::to_value(block).ok()),
            cors: route
                .cors
                .and_then(|block| serde_json::to_value(block).ok()),
        }
    }
}

impl From<RouteRecord> for RouteDto {
    fn from(record: RouteRecord) -> Self {
        let mut dto = Self::from(record.route);
        dto.enabled = record.enabled;
        dto
    }
}

/// `POST /oagw/v1/routes/{id}/status` payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteStatusRequestDto {
    /// `false` excludes the route from matching.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

/// `OData` list parameters of `GET /oagw/v1/{upstreams,routes}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct ListParamsDto {
    /// `$filter` — a single `field eq|ne 'value'` comparison.
    #[serde(default, rename = "$filter")]
    pub filter: Option<String>,
    /// `$select` — comma-separated field projection.
    #[serde(default, rename = "$select")]
    pub select: Option<String>,
    /// `$orderby` — `field [asc|desc]`.
    #[serde(default, rename = "$orderby")]
    pub orderby: Option<String>,
    /// `$top` — page size, 50 by default, at most 100.
    #[serde(default, rename = "$top")]
    pub top: Option<String>,
    /// `$skip` — number of leading items to drop.
    #[serde(default, rename = "$skip")]
    pub skip: Option<String>,
}

impl From<ListParamsDto> for ListParams {
    fn from(dto: ListParamsDto) -> Self {
        Self {
            filter: dto.filter,
            select: dto.select,
            orderby: dto.orderby,
            top: dto.top,
            skip: dto.skip,
        }
    }
}
