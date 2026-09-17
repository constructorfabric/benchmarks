//! Management-API request DTOs and list helpers.
//!
//! Request bodies mirror the resource schemas minus the read-only fields
//! (`id`, `tenant_id`; `upstream_id` for route replacement). All create
//! bodies reject unknown fields — the schemas declare
//! `additionalProperties: false`.

use serde::Deserialize;
use toolkit_macros::api_dto;

use crate::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, RateLimitConfig, RouteMatch,
    UpstreamServer,
};

fn default_enabled() -> bool {
    true
}

/// Create/replace body for an upstream.
#[derive(Debug, Clone)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamCreate {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Explicit alias; required for IP-based or non-derivable endpoints.
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    pub protocol: String,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// Create body for a route (carries `upstream_id`).
#[derive(Debug, Clone)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteCreate {
    #[serde(default)]
    pub tags: Vec<String>,
    pub upstream_id: uuid::Uuid,
    #[serde(rename = "match")]
    pub r#match: RouteMatch,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
}

/// Replace body for a route. `upstream_id` is immutable and absent.
#[derive(Debug, Clone)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RoutePut {
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(rename = "match")]
    pub r#match: RouteMatch,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
}

/// Create body for a custom plugin.
#[derive(Debug, Clone)]
#[api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginCreate {
    pub name: String,
    /// Full plugin type GTS identifier.
    pub plugin_type: String,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Starlark source for custom plugins.
    #[serde(default)]
    pub source: Option<String>,
}

/// List-query parameters (`$filter`, `$select`, `$orderby`, `$top`, `$skip`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    /// Max results (default 50, max 100).
    #[serde(rename = "$top")]
    pub top: Option<u64>,
    /// Offset for pagination.
    #[serde(rename = "$skip")]
    pub skip: Option<u64>,
}

impl ListQuery {
    /// Resolved page size within [1, 100], defaulting to 50.
    pub fn limit(&self) -> u64 {
        self.top.map(|t| t.clamp(1, 100)).unwrap_or(50)
    }

    /// Resolved offset, defaulting to 0.
    pub fn offset(&self) -> u64 {
        self.skip.unwrap_or(0)
    }
}

/// A single decoded `$filter` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterExpr {
    /// `alias eq '...'`
    AliasEq(String),
    /// `upstream_id eq '...'`
    UpstreamIdEq(uuid::Uuid),
    /// `type eq '...'`
    TypeEq(String),
    /// `enabled eq true|false`
    EnabledEq(bool),
    /// `name eq '...'` (plugins)
    NameEq(String),
    /// Unsupported expression — ignored for list purposes.
    Unsupported,
}

/// Parse the limited `$filter` grammar supported by the OAGW list
/// endpoints. Unsupported expressions are ignored (never fatal).
pub fn parse_filter(raw: Option<&str>) -> Option<FilterExpr> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let (field, value) = raw.split_once(" eq ")?;
    let field = field.trim();
    let value = value.trim().trim_matches('\'');
    let expr = match field {
        "alias" => FilterExpr::AliasEq(value.to_string()),
        "upstream_id" => uuid::Uuid::parse_str(value)
            .map(FilterExpr::UpstreamIdEq)
            .unwrap_or(FilterExpr::Unsupported),
        "type" => FilterExpr::TypeEq(value.to_string()),
        "name" => FilterExpr::NameEq(value.to_string()),
        "enabled" => match value {
            "true" => FilterExpr::EnabledEq(true),
            "false" => FilterExpr::EnabledEq(false),
            _ => FilterExpr::Unsupported,
        },
        _ => FilterExpr::Unsupported,
    };
    Some(expr)
}

/// Sort direction parsed from `$orderby`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Parse `$orderby` (`field asc|desc`, comma-separated; only the first
/// supported field is honored).
pub fn parse_orderby(raw: Option<&str>) -> Option<(String, SortDir)> {
    let raw = raw?.trim();
    let first = raw.split(',').next()?.trim();
    if first.is_empty() {
        return None;
    }
    let (field, dir) = match first.split_once(' ') {
        Some((f, d)) if d.eq_ignore_ascii_case("desc") => (f.trim(), SortDir::Desc),
        Some((f, _)) => (f.trim(), SortDir::Asc),
        None => (first, SortDir::Asc),
    };
    match field {
        "created_at" | "updated_at" | "alias" | "name" | "id" => {
            Some((field.to_string(), dir))
        }
        _ => None,
    }
}

/// OData-style list envelope.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ListResponse<T> {
    pub value: Vec<T>,
    pub total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
