//! Request payloads accepted by the management API.
//!
//! These mirror the published JSON schemas. `id` is accepted and ignored so a
//! client can round-trip a `GET` response straight back into a `PUT`.

use serde::Deserialize;
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginKind, PluginPhase, PluginsConfig,
    Protocol, RateLimitConfig, ServerConfig, default_true,
};

/// `POST`/`PUT` body for `/oagw/v1/upstreams`.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpstreamInput {
    /// Server-generated; ignored on write.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Server-owned; ignored on write. Accepted so a `GET` response can be
    /// sent straight back as a `PUT` body.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,
    #[serde(default = "default_true")]
    pub enabled: bool,
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

/// `POST`/`PUT` body for `/oagw/v1/routes`.
///
/// `upstream_id` is required on create and immutable on replace.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct RouteInput {
    /// Server-generated; ignored on write.
    #[serde(default)]
    pub id: Option<Uuid>,
    /// Server-owned; ignored on write.
    #[serde(default)]
    pub tenant_id: Option<Uuid>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    pub r#match: MatchConfig,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// `POST` body for `/oagw/v1/plugins`. Plugins are immutable — there is no
/// replace form.
#[derive(Debug, Clone, Deserialize, utoipa::ToSchema)]
pub struct PluginInput {
    /// Server-generated; ignored on write.
    #[serde(default)]
    pub id: Option<Uuid>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "plugin_type", alias = "type")]
    pub plugin_type: PluginKind,
    #[serde(default)]
    pub phases: Vec<PluginPhase>,
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub source_code: String,
}

/// Query parameters shared by the list endpoints (`docs/DESIGN.md` §3.3).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    #[serde(rename = "$top")]
    pub top: Option<usize>,
    #[serde(rename = "$skip")]
    pub skip: Option<usize>,
}

// The `OperationBuilder` request-body helpers gate on these markers so only
// vetted DTOs can be published as schemas.
impl toolkit::api::api_dto::RequestApiDto for UpstreamInput {}
impl toolkit::api::api_dto::RequestApiDto for RouteInput {}
impl toolkit::api::api_dto::RequestApiDto for PluginInput {}
