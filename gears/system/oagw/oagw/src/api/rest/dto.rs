//! Wire DTOs of the OAGW management API.
//!
//! Request DTOs are `deny_unknown_fields` and never carry server-generated
//! fields (`id`, `tenant_id`, `created_at`, `updated_at`); the route replace
//! DTO additionally omits `upstream_id`, which is immutable. Response DTOs
//! mirror the domain model 1:1 and add the RFC 3339 timestamps the model keeps
//! as `SystemTime`.
//!
//! Nested configuration sections are projected into the OpenAPI document as
//! free-form objects (`#[schema(value_type = Object)]`): their shape is pinned
//! by `docs/schemas/upstream.v1.schema.json` and `route.v1.schema.json`, which
//! the domain model mirrors field-for-field, so the schema stays accurate
//! without duplicating every section type.
//!
//! List endpoints return [`toolkit::Page`] envelopes. Because `$select`
//! projects fields out of the serialised items, page items are emitted as the
//! projected JSON objects while the OpenAPI document describes the full item
//! schema ([`UpstreamDto`], [`RouteDto`], [`PluginDto`]).

use uuid::Uuid;

use crate::domain::audit::format_rfc3339;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, Plugin, PluginConfig, Protocol, RateLimitConfig, Route,
    RouteMatch, ServerConfig, Upstream, empty_json_object,
};
use crate::domain::validation::{PluginInput, RouteInput, UpstreamInput};

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Body of `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
///
/// `alias` is optional: hostname-based endpoint pools always derive it, and a
/// supplied alias must match the derived value (DESIGN section 3.1). `PUT`
/// replaces the whole resource; omitted optional fields are cleared.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    /// Explicit alias; required for IP-based or non-derivable pools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = String, nullable = true)]
    pub alias: Option<String>,
    /// Defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = bool, nullable = true)]
    pub enabled: Option<bool>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool (`server.endpoints`).
    #[schema(value_type = Object)]
    pub server: ServerConfig,
    /// Upstream protocol; defaults to
    /// `gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1`.
    #[serde(default)]
    #[schema(value_type = String)]
    pub protocol: Protocol,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules; defaults to no rules.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub headers: HeadersConfig,
    /// Plugin chain; defaults to an empty chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: PluginConfig,
    /// Rate-limit budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub cors: Option<CorsConfig>,
}

impl UpstreamRequest {
    /// View of the request as the validator input.
    #[must_use]
    pub fn as_input(&self) -> UpstreamInput {
        UpstreamInput {
            alias: self.alias.clone(),
            enabled: self.enabled,
            tags: self.tags.clone(),
            server: self.server.clone(),
            protocol: self.protocol,
            auth: self.auth.clone(),
            headers: self.headers.clone(),
            plugins: self.plugins.clone(),
            rate_limit: self.rate_limit.clone(),
            cors: self.cors.clone(),
        }
    }
}

/// Wire representation of an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(deny_unknown_fields)]
pub struct UpstreamDto {
    /// GTS instance id, e.g. `gts.cf.core.oagw.upstream.v1~{uuid}`.
    pub id: Uuid,
    /// Routing alias, unique per tenant.
    pub alias: String,
    /// Disabled upstreams reject every request.
    pub enabled: bool,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool (`server.endpoints`).
    #[schema(value_type = Object)]
    pub server: ServerConfig,
    /// Upstream protocol.
    #[schema(value_type = String)]
    pub protocol: Protocol,
    /// Auth plugin binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[schema(value_type = Object)]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[schema(value_type = Object)]
    pub plugins: PluginConfig,
    /// Rate-limit budget.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub cors: Option<CorsConfig>,
    /// Creation instant, RFC 3339 UTC.
    pub created_at: String,
    /// Last modification instant, RFC 3339 UTC.
    pub updated_at: String,
}

impl From<&Upstream> for UpstreamDto {
    fn from(upstream: &Upstream) -> Self {
        Self {
            id: upstream.id,
            alias: upstream.alias.clone(),
            enabled: upstream.enabled,
            tags: upstream.tags.clone(),
            server: upstream.server.clone(),
            protocol: upstream.protocol,
            auth: upstream.auth.clone(),
            headers: upstream.headers.clone(),
            plugins: upstream.plugins.clone(),
            rate_limit: upstream.rate_limit.clone(),
            cors: upstream.cors.clone(),
            created_at: format_rfc3339(upstream.created_at),
            updated_at: format_rfc3339(upstream.updated_at),
        }
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Body of `POST /oagw/v1/routes`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreateRouteRequest {
    /// Owning upstream; must belong to the calling tenant.
    pub upstream_id: Uuid,
    /// Match rules; exactly one of `http` / `grpc`.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub r#match: RouteMatch,
    /// Header transformation overrides; defaults to no rules.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub headers: HeadersConfig,
    /// Plugin chain; defaults to an empty chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: PluginConfig,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub cors: Option<CorsConfig>,
    /// Defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = bool, nullable = true)]
    pub enabled: Option<bool>,
    /// Match priority; defaults to `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = i32, nullable = true)]
    pub priority: Option<i32>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl CreateRouteRequest {
    /// View of the request as the validator input.
    #[must_use]
    pub fn as_input(&self) -> RouteInput {
        RouteInput {
            r#match: self.r#match.clone(),
            headers: self.headers.clone(),
            plugins: self.plugins.clone(),
            rate_limit: self.rate_limit.clone(),
            cors: self.cors.clone(),
            enabled: self.enabled,
            priority: self.priority,
            tags: self.tags.clone(),
        }
    }
}

/// Body of `PUT /oagw/v1/routes/{id}`.
///
/// `upstream_id` is deliberately absent: it is immutable and the service keeps
/// the stored value (DESIGN section 3.3 "PUT (Replace)").
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct ReplaceRouteRequest {
    /// Match rules; exactly one of `http` / `grpc`.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub r#match: RouteMatch,
    /// Header transformation overrides; defaults to no rules.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub headers: HeadersConfig,
    /// Plugin chain; defaults to an empty chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: PluginConfig,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub cors: Option<CorsConfig>,
    /// Defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = bool, nullable = true)]
    pub enabled: Option<bool>,
    /// Match priority; defaults to `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = i32, nullable = true)]
    pub priority: Option<i32>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl ReplaceRouteRequest {
    /// View of the request as the validator input.
    #[must_use]
    pub fn as_input(&self) -> RouteInput {
        RouteInput {
            r#match: self.r#match.clone(),
            headers: self.headers.clone(),
            plugins: self.plugins.clone(),
            rate_limit: self.rate_limit.clone(),
            cors: self.cors.clone(),
            enabled: self.enabled,
            priority: self.priority,
            tags: self.tags.clone(),
        }
    }
}

/// Wire representation of a route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(deny_unknown_fields)]
pub struct RouteDto {
    /// GTS instance id, e.g. `gts.cf.core.oagw.route.v1~{uuid}`.
    pub id: Uuid,
    /// Owning upstream instance id.
    pub upstream_id: Uuid,
    /// Match rules.
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub r#match: RouteMatch,
    /// Header transformation overrides.
    #[schema(value_type = Object)]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[schema(value_type = Object)]
    pub plugins: PluginConfig,
    /// Route-level rate-limit override.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object, nullable = true)]
    pub cors: Option<CorsConfig>,
    /// Disabled routes never match.
    pub enabled: bool,
    /// Match priority; higher wins.
    pub priority: i32,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Creation instant, RFC 3339 UTC.
    pub created_at: String,
    /// Last modification instant, RFC 3339 UTC.
    pub updated_at: String,
}

impl From<&Route> for RouteDto {
    fn from(route: &Route) -> Self {
        Self {
            id: route.id,
            upstream_id: route.upstream_id,
            r#match: route.r#match.clone(),
            headers: route.headers.clone(),
            plugins: route.plugins.clone(),
            rate_limit: route.rate_limit.clone(),
            cors: route.cors.clone(),
            enabled: route.enabled,
            priority: route.priority,
            tags: route.tags.clone(),
            created_at: format_rfc3339(route.created_at),
            updated_at: format_rfc3339(route.updated_at),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Body of `POST /oagw/v1/plugins`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct CreatePluginRequest {
    /// Plugin base type GTS id, e.g. `gts.cf.core.oagw.guard_plugin.v1`.
    pub plugin_type: String,
    /// Plugin configuration payload; defaults to `{}`.
    #[serde(default = "empty_json_object")]
    pub config: serde_json::Value,
    /// Defaults to `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = bool, nullable = true)]
    pub enabled: Option<bool>,
    /// Discovery tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl CreatePluginRequest {
    /// View of the request as the validator input.
    #[must_use]
    pub fn as_input(&self) -> PluginInput {
        PluginInput {
            plugin_type: self.plugin_type.clone(),
            config: self.config.clone(),
            enabled: self.enabled,
            tags: self.tags.clone(),
        }
    }
}

/// Wire representation of a plugin.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(deny_unknown_fields)]
pub struct PluginDto {
    /// GTS instance id, e.g. `gts.cf.core.oagw.plugin.v1~{uuid}`.
    pub id: Uuid,
    /// Plugin base type GTS id.
    pub plugin_type: String,
    /// Plugin configuration payload.
    pub config: serde_json::Value,
    /// Disabled plugins are skipped by the chain builder instead of failing the
    /// upstream with `503 PluginNotFound`.
    pub enabled: bool,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Creation instant, RFC 3339 UTC.
    pub created_at: String,
    /// Last modification instant, RFC 3339 UTC.
    pub updated_at: String,
}

impl From<&Plugin> for PluginDto {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: plugin.id,
            plugin_type: plugin.plugin_type.clone(),
            config: plugin.config.clone(),
            enabled: plugin.enabled,
            tags: plugin.tags.clone(),
            created_at: format_rfc3339(plugin.created_at),
            updated_at: format_rfc3339(plugin.updated_at),
        }
    }
}

/// Body of `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(deny_unknown_fields)]
pub struct PluginSourceDto {
    /// Plugin GTS instance id.
    pub plugin_id: Uuid,
    /// Plugin base type GTS id.
    pub plugin_type: String,
    /// Deterministic Starlark rendering of the plugin definition.
    pub source: String,
}

// ---------------------------------------------------------------------------
// List envelopes
// ---------------------------------------------------------------------------

/// OpenAPI projection of the platform page envelope for upstreams.
///
/// The runtime returns `toolkit::Page<UpstreamDto>`; this type declares the
/// same `{ items, page_info }` shape for the OpenAPI document because
/// `toolkit-odata` only implements `ToSchema` for `Page<T>` behind its
/// `with-utoipa` feature, which this crate does not enable.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamPageDto {
    /// One page of upstreams, after `$filter`, `$orderby`, `$skip` and `$top`.
    pub items: Vec<UpstreamDto>,
    /// Envelope metadata.
    pub page_info: PageMetaDto,
}

/// OpenAPI projection of the platform page envelope for routes.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RoutePageDto {
    /// One page of routes.
    pub items: Vec<RouteDto>,
    /// Envelope metadata.
    pub page_info: PageMetaDto,
}

/// OpenAPI projection of the platform page envelope for plugins.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginPageDto {
    /// One page of plugins.
    pub items: Vec<PluginDto>,
    /// Envelope metadata.
    pub page_info: PageMetaDto,
}

/// Metadata of the platform page envelope.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PageMetaDto {
    /// Cursor of the next page; always `null`, OAGW pages with offsets.
    pub next_cursor: Option<String>,
    /// Cursor of the previous page; always `null`, OAGW pages with offsets.
    pub prev_cursor: Option<String>,
    /// Effective page size (`$top` after clamping).
    pub limit: u64,
}

// ---------------------------------------------------------------------------
// Sharing modes
// ---------------------------------------------------------------------------
/// `true` when any hierarchical section of `upstream` is `enforce`, which
/// blocks a descendant from overriding it (DESIGN "Hierarchical
/// Configuration").
///
/// The rule lives in the domain layer next to the bind check that uses it; it
/// is re-exported here because the OpenAPI document and the
/// `X-OAGW-*` semantics of the wire DTOs depend on it.
#[doc(inline)]
pub use crate::domain::model::enforces_override;

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
