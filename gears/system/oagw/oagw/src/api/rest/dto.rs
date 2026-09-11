//! REST DTOs.
//!
//! Resource identifiers on the wire are anonymous GTS identifiers
//! (`gts.cf.core.oagw.upstream.v1~<uuid>`, DESIGN "Resource Identification
//! Pattern"); a bare UUID is also accepted on input so a client that stored
//! the raw id keeps working.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, Plugin, PluginKind, PluginPhase,
    PluginsConfig, RateLimitConfig, Route, ServerConfig, Upstream,
};
use crate::domain::services::management::{PluginInput, RouteInput, UpstreamInput};

/// Upstream, on the wire. The same shape serves create, replace and read:
/// server-assigned members are ignored on input.
#[toolkit_macros::api_dto(request, response)]
pub struct UpstreamDto {
    /// Anonymous GTS identifier; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Owning tenant; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Enable flag; defaults to `true` on create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Routing alias. Auto-derived for hostname pools; required for
    /// IP-based or otherwise non-derivable pools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerConfig>,
    /// Protocol GTS identifier. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Guard / transform chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp (RFC 3339, UTC); server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last-modification timestamp (RFC 3339, UTC); server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl UpstreamDto {
    /// Project a stored upstream onto the wire.
    #[must_use]
    pub fn from_domain(upstream: &Upstream) -> Self {
        Self {
            id: Some(gts::anonymous_id(gts::UPSTREAM_TYPE, upstream.id)),
            tenant_id: Some(upstream.tenant_id.to_string()),
            enabled: Some(upstream.spec.enabled),
            alias: Some(upstream.spec.alias.clone()),
            tags: upstream.spec.tags.clone(),
            server: Some(upstream.spec.server.clone()),
            protocol: Some(upstream.spec.protocol.clone()),
            auth: upstream.spec.auth.clone(),
            headers: upstream.spec.headers.clone(),
            plugins: upstream.spec.plugins.clone(),
            rate_limit: upstream.spec.rate_limit.clone(),
            cors: upstream.spec.cors.clone(),
            created_at: Some(upstream.created_at.clone()),
            updated_at: Some(upstream.updated_at.clone()),
        }
    }

    /// Convert a request body into the Control Plane input.
    #[must_use]
    pub fn into_input(self) -> UpstreamInput {
        UpstreamInput {
            enabled: self.enabled,
            alias: self.alias,
            tags: self.tags,
            server: self.server,
            protocol: self.protocol,
            auth: self.auth,
            headers: self.headers,
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            cors: self.cors,
        }
    }
}

/// Route, on the wire.
#[toolkit_macros::api_dto(request, response)]
pub struct RouteDto {
    /// Anonymous GTS identifier; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Owning tenant; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Parent upstream. Required on create, immutable afterwards. Accepts
    /// either the anonymous GTS identifier or a bare UUID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Enable flag; defaults to `true` on create.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Match priority; defaults to `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Inbound match rules. Required.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub match_config: Option<MatchConfig>,
    /// Which protocol block `match` carries; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_type: Option<String>,
    /// Guard / transform chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
    /// Creation timestamp (RFC 3339, UTC); server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last-modification timestamp (RFC 3339, UTC); server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl RouteDto {
    /// Project a stored route onto the wire.
    #[must_use]
    pub fn from_domain(route: &Route) -> Self {
        let match_type = match (&route.spec.match_config.http, &route.spec.match_config.grpc) {
            (Some(_), _) => Some("http".to_owned()),
            (None, Some(_)) => Some("grpc".to_owned()),
            (None, None) => None,
        };
        Self {
            id: Some(gts::anonymous_id(gts::ROUTE_TYPE, route.id)),
            tenant_id: Some(route.tenant_id.to_string()),
            upstream_id: Some(gts::anonymous_id(gts::UPSTREAM_TYPE, route.upstream_id)),
            enabled: Some(route.spec.enabled),
            priority: Some(route.spec.priority),
            tags: route.spec.tags.clone(),
            match_config: Some(route.spec.match_config.clone()),
            match_type,
            plugins: route.spec.plugins.clone(),
            rate_limit: route.spec.rate_limit.clone(),
            cors: route.spec.cors.clone(),
            created_at: Some(route.created_at.clone()),
            updated_at: Some(route.updated_at.clone()),
        }
    }

    /// Convert a request body into the Control Plane input.
    ///
    /// # Errors
    ///
    /// Returns `400` when `upstream_id` is present but is neither an
    /// anonymous upstream identifier nor a UUID.
    pub fn into_input(self) -> Result<RouteInput, crate::domain::error::OagwError> {
        let upstream_id = match self.upstream_id.as_deref() {
            None => None,
            Some(raw) => Some(gts::parse_resource_id(gts::UPSTREAM_TYPE, raw).ok_or_else(
                || {
                    crate::domain::error::OagwError::validation(format!(
                        "upstream_id '{raw}' is neither a '{}<uuid>' identifier nor a UUID",
                        gts::UPSTREAM_TYPE
                    ))
                },
            )?),
        };
        Ok(RouteInput {
            enabled: self.enabled,
            priority: self.priority,
            tags: self.tags,
            upstream_id,
            match_config: self.match_config,
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            cors: self.cors,
        })
    }
}

/// Custom plugin, on the wire. `source_code` is write-only: read it back
/// through `GET /oagw/v1/plugins/{id}/source`.
#[toolkit_macros::api_dto(request, response)]
pub struct PluginDto {
    /// Anonymous GTS identifier; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Owning tenant; server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    /// Plugin kind: `auth`, `guard` or `transform`. Also accepts the full
    /// base GTS type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_type: Option<String>,
    /// Tenant-unique name. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Declared transform phases.
    #[serde(default)]
    pub phases: Vec<PluginPhase>,
    /// JSON Schema for the plugin's configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<Value>,
    /// Starlark source. Required on create; never echoed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Creation timestamp (RFC 3339, UTC); server-assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last time the plugin was resolved on the hot path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<String>,
    /// Instant from which an unlinked plugin may be garbage-collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<String>,
}

/// Parse the `plugin_type` member: `auth` / `guard` / `transform`, or the
/// corresponding base GTS type.
#[must_use]
pub fn parse_plugin_kind(raw: &str) -> Option<PluginKind> {
    let trimmed = raw.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "auth" | "auth_plugin" => return Some(PluginKind::Auth),
        "guard" | "guard_plugin" => return Some(PluginKind::Guard),
        "transform" | "transform_plugin" => return Some(PluginKind::Transform),
        _ => {}
    }
    for kind in [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform] {
        let base = kind.base_type();
        if trimmed.eq_ignore_ascii_case(base)
            || trimmed.eq_ignore_ascii_case(base.trim_end_matches('~'))
        {
            return Some(kind);
        }
    }
    // A full anonymous identifier also carries the kind in its base type.
    PluginKind::from_plugin_ref(trimmed)
}

/// Short wire name of a plugin kind.
#[must_use]
pub fn plugin_kind_name(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Auth => "auth",
        PluginKind::Guard => "guard",
        PluginKind::Transform => "transform",
    }
}

impl PluginDto {
    /// Project a stored plugin onto the wire.
    #[must_use]
    pub fn from_domain(plugin: &Plugin) -> Self {
        Self {
            id: Some(plugin.gts_id()),
            tenant_id: Some(plugin.tenant_id.to_string()),
            plugin_type: Some(plugin_kind_name(plugin.kind).to_owned()),
            name: Some(plugin.name.clone()),
            description: plugin.description.clone(),
            phases: plugin.phases.clone(),
            config_schema: plugin.config_schema.clone(),
            // Never echoed: fetch it through `/plugins/{id}/source`.
            source_code: None,
            created_at: Some(plugin.created_at.clone()),
            last_used_at: plugin.last_used_at.clone(),
            gc_eligible_at: plugin.gc_eligible_at.clone(),
        }
    }

    /// Convert a request body into the Control Plane input.
    #[must_use]
    pub fn into_input(self) -> PluginInput {
        PluginInput {
            kind: self.plugin_type.as_deref().and_then(parse_plugin_kind),
            name: self.name,
            description: self.description,
            phases: self.phases,
            config_schema: self.config_schema,
            source_code: self.source_code,
        }
    }
}

/// Response of `GET /oagw/v1/plugins/{id}/source`.
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// Anonymous GTS identifier of the plugin.
    pub id: String,
    /// Plugin kind.
    pub plugin_type: String,
    /// Starlark source.
    pub source_code: String,
}

impl PluginSourceDto {
    /// Project a stored plugin's source onto the wire.
    #[must_use]
    pub fn from_domain(plugin: &Plugin) -> Self {
        Self {
            id: plugin.gts_id(),
            plugin_type: plugin_kind_name(plugin.kind).to_owned(),
            source_code: plugin.source_code.clone(),
        }
    }
}

/// Parse an upstream path parameter.
///
/// # Errors
///
/// Returns `404` for anything that is not an upstream identifier: an id that
/// cannot name a resource cannot name one that exists.
pub fn parse_upstream_id(raw: &str) -> Result<Uuid, crate::domain::error::OagwError> {
    gts::parse_resource_id(gts::UPSTREAM_TYPE, raw).ok_or_else(|| {
        crate::domain::error::OagwError::not_found(format!("upstream '{raw}' not found"))
    })
}

/// Parse a route path parameter.
///
/// # Errors
///
/// Returns `404` for anything that is not a route identifier.
pub fn parse_route_id(raw: &str) -> Result<Uuid, crate::domain::error::OagwError> {
    gts::parse_resource_id(gts::ROUTE_TYPE, raw).ok_or_else(|| {
        crate::domain::error::OagwError::not_found(format!("route '{raw}' not found"))
    })
}

/// Parse a plugin path parameter. The kind is carried by the identifier's
/// base type, so all three plugin families are accepted.
///
/// # Errors
///
/// Returns `404` for anything that is not a plugin identifier.
pub fn parse_plugin_id(raw: &str) -> Result<Uuid, crate::domain::error::OagwError> {
    for kind in [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform] {
        if let Some(uuid) = gts::parse_resource_id(kind.base_type(), raw) {
            return Ok(uuid);
        }
    }
    Err(crate::domain::error::OagwError::not_found(format!(
        "plugin '{raw}' not found"
    )))
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod tests;
