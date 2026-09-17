//! Management-plane request DTOs and wire entity views.
//!
//! Request bodies mirror the GTS schemas (`docs/schemas/*.v1.schema.json`,
//! `snake_case` as written in those files, `additionalProperties: false`);
//! `#[api_dto(request)]` derives serde `Deserialize` + `utoipa::ToSchema`
//! with a `snake_case` container rename. Response entities are the
//! schema-shaped entity bodies prefixed with the anonymous GTS resource id
//! (`gts.cf.core.oagw.{type}.v1~{uuid}` — DESIGN §3.3); the internal
//! `tenant_id` and (for upstreams/routes) the server-assigned `id` never
//! surface in the entity body.

use serde::Serialize;
use toolkit_macros::api_dto;
use uuid::Uuid;

use crate::domain::models::{
    AuthConfig, CorsConfig, HeadersConfig, Plugin, PluginInput, PluginKind, PluginsConfig,
    RateLimitConfig, Route, RouteInput, RouteMatch, ServerConfig, Upstream, UpstreamInput,
    UpstreamProtocol,
};
use crate::gts_helpers;

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Request body for creating or replacing an upstream (DESIGN §3.3).
///
/// `alias` is optional: hostname endpoints derive it automatically (a
/// matching explicit alias is an accepted idempotent no-op, a conflicting
/// one is rejected); IP-based or otherwise non-derivable endpoint sets
/// require it.
#[derive(Debug, Clone)]
#[api_dto(request)]
pub struct UpstreamRequest {
    /// Whether the upstream is enabled (default `true`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional explicit routing alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Upstream server block (one or more endpoints).
    pub server: ServerConfig,
    /// Connection protocol (GTS instance identifier).
    pub protocol: UpstreamProtocol,
    /// Upstream authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Request/response header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain applied to proxied traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration (ADR 0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<UpstreamRequest> for UpstreamInput {
    fn from(request: UpstreamRequest) -> Self {
        Self {
            enabled: request.enabled,
            alias: request.alias,
            tags: request.tags,
            server: request.server,
            protocol: request.protocol,
            auth: request.auth,
            headers: request.headers,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Request body for creating a route (DESIGN §3.3).
#[derive(Debug, Clone)]
#[api_dto(request)]
pub struct RouteRequest {
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Reference to the upstream this route binds (must belong to the
    /// calling tenant).
    pub upstream_id: Uuid,
    /// Protocol-scoped inbound matching rules (`http` / `grpc`).
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Plugin chain applied to matching traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration (ADR 0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<RouteRequest> for RouteInput {
    fn from(request: RouteRequest) -> Self {
        Self {
            tags: request.tags,
            upstream_id: request.upstream_id,
            match_: request.match_,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

/// Request body for replacing a route (DESIGN §3.3).
///
/// `upstream_id` is immutable and therefore absent from the update DTO;
/// the handler re-binds the existing route's upstream.
#[derive(Debug, Clone)]
#[api_dto(request)]
pub struct RouteUpdateRequest {
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Protocol-scoped inbound matching rules (`http` / `grpc`).
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    /// Plugin chain applied to matching traffic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsConfig>,
    /// Rate-limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration (ADR 0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl RouteUpdateRequest {
    /// Convert into a route input, keeping the route's immutable upstream
    /// binding.
    #[must_use]
    pub fn into_input(self, upstream_id: Uuid) -> RouteInput {
        RouteInput {
            tags: self.tags,
            upstream_id,
            match_: self.match_,
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            cors: self.cors,
        }
    }
}

// ---------------------------------------------------------------------------
// Custom plugin definition
// ---------------------------------------------------------------------------

/// Request body for creating a custom plugin (DESIGN §3.3).
///
/// Plugins are immutable after creation (no `PUT`); the request picks a
/// builtin implementation (`builtinType`, a GTS identifier) and the
/// configuration that parameterizes it.
#[derive(Debug, Clone)]
#[api_dto(request)]
pub struct PluginRequest {
    /// Plugin kind — `auth` / `guard` / `transform` (wire field `type`).
    #[serde(rename = "type")]
    pub kind: PluginKind,
    /// GTS identifier of the builtin implementation this plugin
    /// parameterizes (wire field `builtinType`, matching the entity body
    /// serialization of the `Plugin` model).
    #[serde(rename = "builtinType")]
    pub builtin_type: String,
    /// Human-readable name (unique per tenant).
    pub name: String,
    /// Effective configuration for the builtin implementation.
    #[serde(default)]
    pub config: serde_json::Value,
}

impl From<PluginRequest> for PluginInput {
    fn from(request: PluginRequest) -> Self {
        Self {
            kind: request.kind,
            builtin_type: request.builtin_type,
            name: request.name,
            config: request.config,
        }
    }
}

// ---------------------------------------------------------------------------
// Entity views
// ---------------------------------------------------------------------------

/// Anonymous GTS resource id for a management entity:
/// `gts.cf.core.oagw.{type}.v1~{uuid}` (DESIGN §3.3).
#[must_use]
pub fn entity_id(resource_type: &str, id: Uuid) -> String {
    format!("{resource_type}{id}")
}

/// Wire view of a management entity: the GTS resource id plus the
/// schema-shaped entity body (internal bookkeeping fields never surface).
#[derive(Debug, Clone, Serialize)]
pub struct EntityView {
    /// Anonymous GTS identifier of the resource.
    pub id: String,
    /// Schema-shaped entity body.
    #[serde(flatten)]
    pub fields: serde_json::Value,
}

impl EntityView {
    /// View of an [`Upstream`].
    #[must_use]
    pub fn upstream(upstream: &Upstream) -> Self {
        Self {
            id: entity_id(gts_helpers::UPSTREAM_TYPE_ID, upstream.id),
            fields: value_of(upstream),
        }
    }

    /// View of a [`Route`].
    #[must_use]
    pub fn route(route: &Route) -> Self {
        Self {
            id: entity_id(gts_helpers::ROUTE_TYPE_ID, route.id),
            fields: value_of(route),
        }
    }

    /// View of a [`Plugin`].
    ///
    /// The stored plugin serializes its own UUID `id`; the wire view
    /// replaces it with the GTS resource id.
    #[must_use]
    pub fn plugin(plugin: &Plugin) -> Self {
        let mut fields = value_of(plugin);
        if let Some(object) = fields.as_object_mut() {
            object.remove("id");
        }
        Self {
            id: entity_id(gts_helpers::PLUGIN_TYPE_ID, plugin.id),
            fields,
        }
    }
}

fn value_of(value: &impl Serialize) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{Endpoint, EndpointScheme, ServerConfig};

    fn upstream() -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: "api.openai.com".to_owned(),
            tags: vec!["llm".to_owned()],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: Some(443),
                }],
            },
            protocol: UpstreamProtocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn entity_id_uses_anonymous_gts_identifier() {
        let id = Uuid::new_v4();
        let view = EntityView::upstream(&Upstream {
            id,
            ..upstream()
        });
        assert_eq!(view.id, format!("{}{id}", gts_helpers::UPSTREAM_TYPE_ID));
        assert!(view.id.starts_with("gts.cf.core.oagw.upstream.v1~"));
        // Internal bookkeeping never surfaces in the entity body.
        assert!(view.fields.get("tenant_id").is_none());
        assert!(view.fields.get("id").is_none());
        assert_eq!(
            view.fields.get("alias").and_then(|v| v.as_str()),
            Some("api.openai.com")
        );
    }

    #[test]
    fn plugin_view_replaces_uuid_with_gts_id() {
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            kind: PluginKind::Guard,
            builtin_type: crate::gts_helpers::GUARD_PLUGIN_REQUIRED_HEADERS.to_owned(),
            name: "guardian".to_owned(),
            config: serde_json::json!({}),
        };
        let view = EntityView::plugin(&plugin);
        assert_eq!(view.id, format!("{}{}", gts_helpers::PLUGIN_TYPE_ID, plugin.id));
        // Wire `type` (not `kind`) per the `$filter=type eq 'guard'` surface.
        assert_eq!(view.fields.get("type").and_then(|v| v.as_str()), Some("guard"));
        assert!(view.fields.get("kind").is_none());
        assert!(view.fields.get("id").is_none());
    }

    #[test]
    fn upstream_request_converts_to_input() {
        let raw = serde_json::json!({
            "enabled": false,
            "tags": ["llm"],
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com" }] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        });
        let request: UpstreamRequest = serde_json::from_value(raw).expect("parses");
        let input: UpstreamInput = request.into();
        assert!(!input.enabled);
        assert_eq!(input.alias, None);
        assert_eq!(input.tags, vec!["llm"]);
        assert_eq!(input.server.endpoints.len(), 1);
        assert_eq!(input.protocol, UpstreamProtocol::Http);
    }

    #[test]
    fn route_update_request_rebinds_upstream_id() {
        let raw = serde_json::json!({
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let request: RouteUpdateRequest = serde_json::from_value(raw).expect("parses");
        let upstream_id = Uuid::new_v4();
        let input = request.into_input(upstream_id);
        assert_eq!(input.upstream_id, upstream_id);
        assert_eq!(input.match_.http.as_ref().unwrap().path, "/v1");
    }
}
