//! Wire DTOs of the management and proxy APIs.
//!
//! The transport shapes are deliberately thin: nested value types are the
//! domain value types of [`crate::domain::model`], which already carry the
//! validation rules of `docs/schemas/` (`deny_unknown_fields`, enums,
//! defaults). Only the top level carries the transport marker traits.

use crate::domain::dto::{ListQuery, ProxyContext as DomainProxyContext};
use crate::domain::error::DomainError;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, Protocol, RateLimitConfig, Route,
    ServerConfig, Upstream, default_enabled,
};
/// Media type of the Starlark source endpoint.
pub const STARLARK_MEDIA_TYPE: &str = "text/x-starlark";

/// Create or replace an upstream.
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequestDto {
    /// Explicit alias; required when the endpoint pool does not derive one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Upstream protocol, as a GTS id.
    pub protocol: Protocol,
    /// Disabled upstreams reject every request.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Outbound auth configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Upstream plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Flat categorization tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl UpstreamRequestDto {
    /// Project the wire payload onto a domain command.
    #[must_use]
    pub fn into_command(self) -> crate::domain::dto::UpstreamCommand {
        crate::domain::dto::UpstreamCommand {
            alias: self.alias,
            protocol: self.protocol,
            enabled: self.enabled,
            server: self.server,
            auth: self.auth,
            headers: self.headers,
            rate_limit: self.rate_limit,
            cors: self.cors,
            plugins: self.plugins,
            tags: self.tags,
        }
    }
}

/// A stored upstream.
#[toolkit_macros::api_dto(response)]
pub struct UpstreamResponseDto {
    /// Anonymous GTS id of the upstream.
    pub id: String,
    /// Routing key used by the proxy API.
    pub alias: String,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Disabled upstreams reject every request.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Outbound auth configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Rate-limit configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Upstream plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Flat categorization tags.
    pub tags: Vec<String>,
    /// Creation instant (RFC 3339, UTC).
    pub created_at: String,
    /// Last write instant (RFC 3339, UTC).
    pub updated_at: String,
}

impl UpstreamResponseDto {
    /// Map a stored upstream onto the wire.
    #[must_use]
    pub fn from_row(row: &Upstream) -> Self {
        Self {
            id: crate::domain::model::resource_gts_id(crate::domain::model::UPSTREAM_TYPE, row.id),
            alias: row.alias.clone(),
            protocol: row.protocol,
            enabled: row.enabled,
            server: row.server.clone(),
            auth: row.auth.clone(),
            headers: row.headers.clone(),
            rate_limit: row.rate_limit.clone(),
            cors: row.cors.clone(),
            plugins: row.plugins.clone(),
            tags: row.tags.clone(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
        }
    }
}

impl From<&Upstream> for UpstreamResponseDto {
    fn from(row: &Upstream) -> Self {
        Self::from_row(row)
    }
}

/// Create or replace a route.
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequestDto {
    /// Owning upstream, as a GTS id; immutable on replace.
    pub upstream_id: String,
    /// Match rules; exactly one of `http`/`grpc`.
    pub r#match: crate::domain::model::MatchConfig,
    /// Higher priority wins when several routes match.
    #[serde(default)]
    pub priority: u32,
    /// Disabled routes never match.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Route-level rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Flat categorization tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl RouteRequestDto {
    /// Parse the upstream GTS id and project onto a domain command.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when `upstream_id` is not an
    /// anonymous GTS id with a UUID tail.
    pub fn into_command(self) -> Result<crate::domain::dto::RouteCommand, DomainError> {
        // A bare UUID would silently bind the route to an upstream of another
        // gear or of no gear at all, so the base type is mandatory.
        let upstream_id = crate::domain::model::parse_typed_resource_id(
            &self.upstream_id,
            crate::domain::model::UPSTREAM_TYPE,
        )?;
        Ok(crate::domain::dto::RouteCommand {
            upstream_id,
            r#match: self.r#match,
            priority: self.priority,
            enabled: self.enabled,
            rate_limit: self.rate_limit,
            cors: self.cors,
            plugins: self.plugins,
            tags: self.tags,
        })
    }
}

/// A stored route.
#[toolkit_macros::api_dto(response)]
pub struct RouteResponseDto {
    /// Anonymous GTS id of the route.
    pub id: String,
    /// Owning upstream, as a GTS id.
    pub upstream_id: String,
    /// Match rules.
    pub r#match: crate::domain::model::MatchConfig,
    /// Higher priority wins when several routes match.
    pub priority: u32,
    /// Disabled routes never match.
    pub enabled: bool,
    /// Route-level rate-limit override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Route-level plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Flat categorization tags.
    pub tags: Vec<String>,
    /// Creation instant (RFC 3339, UTC).
    pub created_at: String,
    /// Last write instant (RFC 3339, UTC).
    pub updated_at: String,
}

impl RouteResponseDto {
    /// Map a stored route onto the wire.
    #[must_use]
    pub fn from_row(row: &Route) -> Self {
        Self {
            id: crate::domain::model::resource_gts_id(crate::domain::model::ROUTE_TYPE, row.id),
            upstream_id: crate::domain::model::resource_gts_id(
                crate::domain::model::UPSTREAM_TYPE,
                row.upstream_id,
            ),
            r#match: row.r#match.clone(),
            priority: row.priority,
            enabled: row.enabled,
            rate_limit: row.rate_limit.clone(),
            cors: row.cors.clone(),
            plugins: row.plugins.clone(),
            tags: row.tags.clone(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
        }
    }
}

/// Create a custom Starlark plugin.
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequestDto {
    /// Plugin kind: `auth`, `guard` or `transform`.
    pub plugin_type: crate::domain::model::PluginType,
    /// Human-readable name, unique per tenant and kind.
    pub name: String,
    /// JSON Schema the plugin `config` must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source.
    pub source_code: String,
    /// Phases the plugin declares.
    #[serde(default)]
    pub phases: Vec<crate::domain::model::PluginPhase>,
}

impl PluginRequestDto {
    /// Project the wire payload onto a domain command.
    #[must_use]
    pub fn into_command(self) -> crate::domain::dto::PluginCommand {
        crate::domain::dto::PluginCommand {
            plugin_type: self.plugin_type,
            name: self.name,
            config_schema: self.config_schema,
            source_code: self.source_code,
            phases: self.phases,
        }
    }
}

/// A stored custom plugin.
#[toolkit_macros::api_dto(response)]
pub struct PluginResponseDto {
    /// Anonymous GTS id of the plugin.
    pub id: String,
    /// Plugin kind.
    pub plugin_type: crate::domain::model::PluginType,
    /// Human-readable name.
    pub name: String,
    /// JSON Schema the plugin `config` must satisfy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Phases the plugin declares.
    pub phases: Vec<crate::domain::model::PluginPhase>,
    /// Creation instant (RFC 3339, UTC).
    pub created_at: String,
    /// Last write instant (RFC 3339, UTC).
    pub updated_at: String,
}

impl PluginResponseDto {
    /// Map a stored plugin onto the wire; the source is served separately.
    #[must_use]
    pub fn from_row(row: &crate::domain::model::Plugin) -> Self {
        Self {
            id: crate::domain::model::resource_gts_id(row.plugin_type.gts_base_type(), row.id),
            plugin_type: row.plugin_type,
            name: row.name.clone(),
            config_schema: row.config_schema.clone(),
            phases: row.phases.clone(),
            created_at: row.created_at.clone(),
            updated_at: row.updated_at.clone(),
        }
    }
}

/// The Starlark source of a custom plugin.
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceResponseDto {
    /// Anonymous GTS id of the plugin.
    pub id: String,
    /// Plugin kind.
    pub plugin_type: crate::domain::model::PluginType,
    /// Starlark source.
    pub source_code: String,
}

/// A page of list results with `$select` applied.
#[toolkit_macros::api_dto(response)]
pub struct ListEnvelopeDto {
    /// Projected rows of the page.
    pub items: Vec<serde_json::Value>,
    /// Page description.
    pub page_info: PageMetaDto,
}

/// Paging metadata of a list response.
#[toolkit_macros::api_dto(response)]
pub struct PageMetaDto {
    /// Page size in effect.
    pub limit: u64,
    /// Offset the page starts at.
    pub skip: u64,
}

/// Parsed `$top`/`$skip` of a list request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageSpec {
    /// Page size.
    pub top: u64,
    /// Offset.
    pub skip: u64,
}

/// Build a [`ListQuery`] from a raw query string.
///
/// `$filter`, `$orderby`, `$select`, `$top` and `$skip` are bound; any other
/// `$`-prefixed key is refused rather than ignored, so a caller never gets a
/// `200` that silently dropped what it asked for.
///
/// # Errors
/// Returns [`DomainError::InvalidArgument`] for an unknown system query
/// option, a malformed expression, or a `$top` outside `1..=max_top`.
pub fn build_list_query(
    raw: Option<&str>,
    default_top: u64,
    max_top: u64,
) -> Result<ListQuery, DomainError> {
    const ACCEPTED: [&str; 5] = ["$filter", "$orderby", "$select", "$top", "$skip"];
    let mut filter: Option<&str> = None;
    let mut orderby: Option<&str> = None;
    let mut select: Option<&str> = None;
    let mut top: Option<u64> = None;
    let mut skip: Option<u64> = None;

    let pairs: Vec<(String, String)> = match raw {
        Some(query) => form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect(),
        None => Vec::new(),
    };

    for (key, value) in &pairs {
        if !key.starts_with('$') {
            continue;
        }
        if !ACCEPTED.contains(&key.as_str()) {
            return Err(DomainError::InvalidArgument {
                detail: format!("unknown query option '{key}'"),
            });
        }
        match key.as_str() {
            "$filter" => filter = Some(value.as_str()),
            "$orderby" => orderby = Some(value.as_str()),
            "$select" => select = Some(value.as_str()),
            "$top" => {
                top = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| DomainError::InvalidArgument {
                            detail: format!("$top must be a positive integer, got '{value}'"),
                        })?,
                );
            }
            "$skip" => {
                skip = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| DomainError::InvalidArgument {
                            detail: format!("$skip must be a non-negative integer, got '{value}'"),
                        })?,
                );
            }
            _ => {}
        }
    }

    let top = match top {
        Some(0) => {
            return Err(DomainError::InvalidArgument {
                detail: "$top must be at least 1".to_owned(),
            });
        }
        Some(value) if value > max_top => {
            return Err(DomainError::InvalidArgument {
                detail: format!("$top must be at most {max_top}"),
            });
        }
        Some(value) => Some(value),
        None => Some(default_top),
    };

    Ok(ListQuery {
        filter: filter.map(crate::domain::dto::parse_filter).transpose()?,
        order: orderby
            .map(crate::domain::dto::parse_order)
            .transpose()?
            .unwrap_or_default(),
        select: select
            .map(crate::domain::dto::parse_select)
            .transpose()?
            .unwrap_or_default(),
        top,
        skip: skip.unwrap_or(0),
    })
}

/// Parse a `{id}` path segment carrying an anonymous GTS id.
///
/// Path parameters use the full anonymous GTS identifier
/// (`DESIGN` §3.3, Resource Identification Pattern), exactly as request bodies
/// do, so a bare UUID never names a resource of an unknown type.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the segment does not carry
/// `base_type` followed by a UUID tail.
fn parse_path_id(raw: &str, accepted: &[&str]) -> Result<uuid::Uuid, DomainError> {
    for base_type in accepted {
        if let Ok(id) = crate::domain::model::parse_typed_resource_id(raw, base_type) {
            return Ok(id);
        }
    }
    let expected = accepted
        .iter()
        .map(|base| format!("{base}<uuid>"))
        .collect::<Vec<_>>()
        .join(" or ");
    Err(DomainError::validation(format!(
        "'{raw}' is not an anonymous GTS resource id of the form {expected}"
    )))
}

/// Parse the `{id}` of an upstream.
///
/// # Errors
/// Returns [`DomainError::Validation`] for a malformed path id.
pub fn parse_upstream_id(raw: &str) -> Result<uuid::Uuid, DomainError> {
    parse_path_id(raw, &[crate::domain::model::UPSTREAM_TYPE])
}

/// Parse the `{id}` of a route.
///
/// # Errors
/// Returns [`DomainError::Validation`] for a malformed path id.
pub fn parse_route_id(raw: &str) -> Result<uuid::Uuid, DomainError> {
    parse_path_id(raw, &[crate::domain::model::ROUTE_TYPE])
}

/// Parse the `{id}` of a custom plugin.
///
/// A plugin id carries the base type of its kind, so all three are accepted.
///
/// # Errors
/// Returns [`DomainError::Validation`] for a malformed path id.
pub fn parse_plugin_id(raw: &str) -> Result<uuid::Uuid, DomainError> {
    use crate::domain::model::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};
    parse_path_id(
        raw,
        &[GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE, AUTH_PLUGIN_TYPE],
    )
}

/// Build the proxy request context of a proxied call.
///
/// The inbound headers are lowercased for the plugin chain, which reads them
/// case-insensitively; a header the HTTP grammar cannot express as UTF-8 is
/// dropped rather than corrupting the context.
#[must_use]
pub fn proxy_context(
    alias: &str,
    path_suffix: Option<&str>,
    method: &axum::http::Method,
    query: Vec<(String, String)>,
    headers: &axum::http::HeaderMap,
    trace_id: Option<String>,
    identity: (uuid::Uuid, uuid::Uuid),
) -> DomainProxyContext {
    DomainProxyContext {
        alias: alias.to_owned(),
        method: method.as_str().to_ascii_uppercase(),
        path: format!("/{}", path_suffix.unwrap_or_default()),
        query,
        headers: headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
            })
            .collect(),
        trace_id,
        tenant: identity.0,
        subject: identity.1,
    }
}

/// The decoded query parameters of a request URI, in wire order.
#[must_use]
pub fn query_of(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    form_urlencoded::parse(raw.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dto_tests.rs"]
mod tests;
