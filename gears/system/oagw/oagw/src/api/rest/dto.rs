// Created: 2026-08-29 by Constructor Tech
//! REST DTOs.
//!
//! The wire shapes are the domain shapes (`docs/schemas/*.json`); DTOs exist to
//! attach OpenAPI metadata and to keep transport concerns out of the domain.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::{ArrayBuilder, ObjectBuilder, Ref, Schema, SchemaType, Type};

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginCreate, PluginDefinition,
    PluginsConfig, RateLimitConfig, Route, RouteCreate, ServerConfig, Upstream, UpstreamCreate,
};

/// Query parameters accepted by the management list endpoints.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    /// OData filter expression.
    #[serde(rename = "$filter", default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Fields to return.
    #[serde(rename = "$select", default, skip_serializing_if = "Option::is_none")]
    pub select: Option<String>,
    /// Sort order, e.g. `created_at desc`.
    #[serde(rename = "$orderby", default, skip_serializing_if = "Option::is_none")]
    pub orderby: Option<String>,
    /// Page size (default 50, max 100).
    #[serde(rename = "$top", default, skip_serializing_if = "Option::is_none")]
    pub top: Option<usize>,
    /// Page offset.
    #[serde(rename = "$skip", default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<usize>,
}

/// Page size default (`$top`).
pub const DEFAULT_PAGE_SIZE: usize = 50;
/// Page size maximum (`$top`).
pub const MAX_PAGE_SIZE: usize = 100;

impl ListQuery {
    /// Effective page size, clamped to `MAX_PAGE_SIZE`.
    #[must_use]
    pub fn page_size(&self) -> usize {
        self.top.unwrap_or(DEFAULT_PAGE_SIZE).min(MAX_PAGE_SIZE)
    }

    /// Effective page offset.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.skip.unwrap_or_default()
    }
}

/// Inline schema for the `toolkit_odata::PageInfo` member of a list envelope.
fn page_info_schema() -> RefOr<Schema> {
    ObjectBuilder::new()
        .property(
            "next_cursor",
            ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::String, Type::Null])),
        )
        .property(
            "prev_cursor",
            ObjectBuilder::new().schema_type(SchemaType::from_iter([Type::String, Type::Null])),
        )
        .property("limit", ObjectBuilder::new().schema_type(Type::Integer))
        .required("limit")
        .into()
}

/// OpenAPI components for the paged list envelopes.
///
/// The handlers return `toolkit_odata::Page<T>` (`items` + `page_info`), the
/// house paged response. `toolkit-odata`'s `with-utoipa` feature is not enabled
/// for this crate, so `Page<T>` itself has no `ToSchema` impl here; each list
/// response registers a locally named component describing the same shape.
macro_rules! paged_list_schema {
    ($name:ident, $item:ty) => {
        /// Schema component of a paged list response (no runtime payload of its own).
        #[derive(Debug, Clone, Copy)]
        pub struct $name;

        impl utoipa::PartialSchema for $name {
            fn schema() -> RefOr<Schema> {
                ObjectBuilder::new()
                    .property(
                        "items",
                        ArrayBuilder::new().items(Ref::from_schema_name(
                            <$item as ToSchema>::name().to_string(),
                        )),
                    )
                    .required("items")
                    .property("page_info", page_info_schema())
                    .required("page_info")
                    .into()
            }
        }

        impl ToSchema for $name {
            fn name() -> std::borrow::Cow<'static, str> {
                std::borrow::Cow::Borrowed(stringify!($name))
            }
        }

        impl toolkit::api::api_dto::ResponseApiDto for $name {}
    };
}

paged_list_schema!(UpstreamList, UpstreamDto);
paged_list_schema!(RouteList, RouteDto);
paged_list_schema!(PluginList, PluginDto);

/// Upstream wire DTO.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UpstreamDto {
    /// System-generated id.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// `true` when the upstream accepts traffic.
    pub enabled: bool,
    /// Normalized routing key.
    pub alias: String,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Auth plugin binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
}

impl From<Upstream> for UpstreamDto {
    fn from(upstream: Upstream) -> Self {
        Self {
            id: upstream.id,
            tenant_id: upstream.tenant_id,
            enabled: upstream.spec.enabled,
            alias: upstream.alias,
            tags: upstream.spec.tags,
            server: upstream.spec.server,
            protocol: upstream.spec.protocol,
            auth: upstream.spec.auth,
            headers: upstream.spec.headers,
            plugins: upstream.spec.plugins,
            rate_limit: upstream.spec.rate_limit,
            cors: upstream.spec.cors,
            created_at: upstream.created_at,
            updated_at: upstream.updated_at,
        }
    }
}

impl From<&Upstream> for UpstreamDto {
    fn from(upstream: &Upstream) -> Self {
        UpstreamDto::from(upstream.clone())
    }
}

/// Route wire DTO.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RouteDto {
    /// System-generated id.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Owning upstream.
    pub upstream_id: uuid::Uuid,
    /// `true` when the route participates in matching.
    pub enabled: bool,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
}

impl From<Route> for RouteDto {
    fn from(route: Route) -> Self {
        Self {
            id: route.id,
            tenant_id: route.tenant_id,
            tags: route.spec.tags,
            upstream_id: route.spec.upstream_id,
            enabled: route.spec.enabled,
            match_config: route.spec.match_config,
            plugins: route.spec.plugins,
            rate_limit: route.spec.rate_limit,
            cors: route.spec.cors,
            created_at: route.created_at,
            updated_at: route.updated_at,
        }
    }
}

impl From<&Route> for RouteDto {
    fn from(route: &Route) -> Self {
        RouteDto::from(route.clone())
    }
}

/// Plugin wire DTO.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PluginDto {
    /// System-generated id.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// `auth_plugin` | `guard_plugin` | `transform_plugin`.
    pub plugin_type: String,
    /// Human readable name.
    pub name: String,
    /// Optional JSON schema for the plugin config.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Creation timestamp.
    pub created_at: String,
    /// Last update timestamp.
    pub updated_at: String,
}

impl From<PluginDefinition> for PluginDto {
    fn from(definition: PluginDefinition) -> Self {
        Self {
            id: definition.id,
            tenant_id: definition.tenant_id,
            plugin_type: definition.plugin_type,
            name: definition.name,
            config_schema: definition.config_schema,
            created_at: definition.created_at,
            updated_at: definition.updated_at,
        }
    }
}

impl From<&PluginDefinition> for PluginDto {
    fn from(definition: &PluginDefinition) -> Self {
        PluginDto::from(definition.clone())
    }
}

/// Request body for `POST /oagw/v1/upstreams`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateUpstreamBody(pub UpstreamCreate);

/// Request body for `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReplaceUpstreamBody(pub UpstreamCreate);

/// Request body for `POST /oagw/v1/routes`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateRouteBody(pub RouteCreate);

/// Request body for `PUT /oagw/v1/routes/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReplaceRouteBody(pub RouteCreate);

/// Request body for `POST /oagw/v1/plugins`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreatePluginBody(pub PluginCreate);

/// Request body for the enable / disable endpoints.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct EnabledBody {
    /// Desired enabled flag.
    pub enabled: bool,
}

/// OpenAPI schema shims for the domain wire shapes.
///
/// The domain model is the wire model (`docs/schemas/*.json`) and it does not
/// carry `utoipa` derives; the management document therefore describes these
/// blocks as free-form JSON objects. Request and response validation stays with
/// serde plus the domain validators, which is strictly stronger than the schema.
macro_rules! schema_as_any_value {
    ($($t:ty),+ $(,)?) => {
        $(
            impl utoipa::PartialSchema for $t {
                fn schema() -> RefOr<Schema> {
                    <serde_json::Value as utoipa::PartialSchema>::schema()
                }
            }

            impl utoipa::ToSchema for $t {}
        )+
    };
}

schema_as_any_value!(
    crate::domain::model::ServerConfig,
    crate::domain::model::AuthConfig,
    crate::domain::model::HeadersConfig,
    crate::domain::model::PluginsConfig,
    crate::domain::model::RateLimitConfig,
    crate::domain::model::CorsConfig,
    crate::domain::model::MatchConfig,
    crate::domain::model::HttpMatch,
    crate::domain::model::GrpcMatch,
    crate::domain::model::UpstreamCreate,
    crate::domain::model::RouteCreate,
    crate::domain::model::PluginCreate,
);

impl toolkit::api::api_dto::ResponseApiDto for UpstreamDto {}
impl toolkit::api::api_dto::ResponseApiDto for RouteDto {}
impl toolkit::api::api_dto::ResponseApiDto for PluginDto {}
impl toolkit::api::api_dto::RequestApiDto for CreateUpstreamBody {}
impl toolkit::api::api_dto::RequestApiDto for ReplaceUpstreamBody {}
impl toolkit::api::api_dto::RequestApiDto for CreateRouteBody {}
impl toolkit::api::api_dto::RequestApiDto for ReplaceRouteBody {}
impl toolkit::api::api_dto::RequestApiDto for CreatePluginBody {}
impl toolkit::api::api_dto::RequestApiDto for EnabledBody {}
