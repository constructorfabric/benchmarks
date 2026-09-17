//! Wire documents (request/response bodies) for the management API.
//!
//! These mirror the resource shapes from `docs/schemas/` and are annotated
//! with `#[toolkit_macros::api_dto]` so they can be used as typed JSON
//! request bodies and response schemas by the REST layer. Conversion from
//! stored records is lossless: `id` and server-generated fields are omitted
//! from request bodies and included in responses.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, MatchConfig, PluginChainConfig,
    PluginDef, PluginKind, Protocol, RateLimitConfig, Route, ServerConfig, Upstream,
};

fn default_true() -> bool {
    true
}

fn is_default_headers(h: &HeadersConfig) -> bool {
    h.request.set.is_empty()
        && h.request.add.is_empty()
        && h.request.remove.is_empty()
        && h.request.passthrough_allowlist.is_empty()
        && h.request.passthrough == super::model::PassthroughMode::None
        && h.response.set.is_empty()
        && h.response.add.is_empty()
        && h.response.remove.is_empty()
}

fn is_default_plugins(p: &PluginChainConfig) -> bool {
    p.items.is_empty() && p.sharing == super::model::SharingMode::Private
}

fn is_default_cors(c: &CorsConfig) -> bool {
    !c.enabled
        && c.allowed_origins.is_empty()
        && c.allowed_methods == ["GET", "POST"]
        && c.expose_headers.is_empty()
        && !c.allow_credentials
        && c.sharing == super::model::SharingMode::Private
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Request body for creating or fully replacing an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpstreamInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit alias. When omitted the alias is auto-derived from the
    /// endpoints (or rejected when not derivable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "is_default_headers")]
    pub headers: HeadersConfig,
    #[serde(default, skip_serializing_if = "is_default_plugins")]
    pub plugins: PluginChainConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "is_default_cors")]
    pub cors: CorsConfig,
}

/// Full wire representation of an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDocument {
    pub id: Uuid,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub alias: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub server: ServerConfig,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    #[serde(default, skip_serializing_if = "is_default_headers")]
    pub headers: HeadersConfig,
    #[serde(default, skip_serializing_if = "is_default_plugins")]
    pub plugins: PluginChainConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default, skip_serializing_if = "is_default_cors")]
    pub cors: CorsConfig,
}

impl From<&Upstream> for UpstreamDocument {
    fn from(u: &Upstream) -> Self {
        UpstreamDocument {
            id: u.id,
            enabled: u.enabled,
            alias: u.alias.clone(),
            tags: u.tags.clone(),
            server: u.server.clone(),
            protocol: u.protocol,
            auth: u.auth.clone(),
            headers: u.headers.clone(),
            plugins: u.plugins.clone(),
            rate_limit: u.rate_limit.clone(),
            cors: u.cors.clone(),
        }
    }
}

/// Upstream summary for OData-style listing.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamSummary {
    pub id: Uuid,
    pub enabled: bool,
    pub alias: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub protocol: Protocol,
}

impl From<&Upstream> for UpstreamSummary {
    fn from(u: &Upstream) -> Self {
        UpstreamSummary {
            id: u.id,
            enabled: u.enabled,
            alias: u.alias.clone(),
            tags: u.tags.clone(),
            protocol: u.protocol,
        }
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Request body for creating or fully replacing a route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct RouteInput {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Target upstream (tenant-scoped). Immutable once set.
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    #[serde(default, skip_serializing_if = "is_default_plugins")]
    pub plugins: PluginChainConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

/// Full wire representation of a route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDocument {
    pub id: Uuid,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    #[serde(default, skip_serializing_if = "is_default_plugins")]
    pub plugins: PluginChainConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl From<&Route> for RouteDocument {
    fn from(r: &Route) -> Self {
        RouteDocument {
            id: r.id,
            enabled: r.enabled,
            tags: r.tags.clone(),
            upstream_id: r.upstream_id,
            match_config: r.match_config.clone(),
            plugins: r.plugins.clone(),
            rate_limit: r.rate_limit.clone(),
        }
    }
}

/// Route summary for listing.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteSummary {
    pub id: Uuid,
    pub enabled: bool,
    pub upstream_id: Uuid,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
}

impl From<&Route> for RouteSummary {
    fn from(r: &Route) -> Self {
        RouteSummary {
            id: r.id,
            enabled: r.enabled,
            upstream_id: r.upstream_id,
            match_config: r.match_config.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Custom plugins
// ---------------------------------------------------------------------------

/// Request body for creating a custom plugin.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct PluginInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub plugin_type: PluginKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
}

/// Full wire representation of a custom plugin.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDocument {
    pub id: Uuid,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub plugin_type: PluginKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Canonical GTS identifier of this plugin.
    pub gts_id: String,
}

impl From<&PluginDef> for PluginDocument {
    fn from(p: &PluginDef) -> Self {
        PluginDocument {
            id: p.id,
            name: p.name.clone(),
            description: p.description.clone(),
            plugin_type: p.plugin_type,
            phases: p.phases.clone(),
            config_schema: p.config_schema.clone(),
            gts_id: p.gts_id(),
        }
    }
}

/// List envelope for upstreams.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamListResponse {
    pub items: Vec<UpstreamSummary>,
    pub total: usize,
}

/// List envelope for routes.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteListResponse {
    pub items: Vec<RouteSummary>,
    pub total: usize,
}

/// List envelope for plugins.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginListResponse {
    pub items: Vec<PluginDocument>,
    pub total: usize,
}

/// Minimal endpoint descriptor (used in error extensions).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EndpointDoc {
    pub scheme: String,
    pub host: String,
    #[serde(default)]
    pub port: u16,
}

impl From<&Endpoint> for EndpointDoc {
    fn from(e: &Endpoint) -> Self {
        EndpointDoc {
            scheme: e.scheme.clone(),
            host: e.host.clone(),
            port: e.port,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::{
        CorsConfig, HeadersConfig, MatchConfig, PluginChainConfig, Route, ServerConfig, Upstream,
    };
    use uuid::Uuid;

    fn uuid() -> Uuid {
        Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap()
    }

    fn upstream() -> Upstream {
        Upstream {
            id: uuid(),
            tenant_id: uuid(),
            enabled: true,
            alias: "api.openai.com".to_owned(),
            tags: vec!["team-x".to_owned()],
            server: ServerConfig {
                endpoints: vec![crate::domain::model::Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginChainConfig::default(),
            rate_limit: None,
            cors: CorsConfig::default(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn upstream_summary() -> UpstreamSummary {
        UpstreamSummary::from(&upstream())
    }

    fn route() -> Route {
        Route {
            id: uuid(),
            tenant_id: uuid(),
            upstream_id: uuid(),
            enabled: true,
            tags: Vec::new(),
            match_config: MatchConfig::Http(crate::domain::model::HttpMatchConfig {
                methods: vec!["GET".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            plugins: PluginChainConfig::default(),
            rate_limit: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn upstream_input_contract() {
        // Full-featured payload deserializes (auth + headers + plugins + CORS).
        let v: UpstreamInput = serde_json::from_value(serde_json::json!({
            "alias": "g",
            "protocol": "http",
            "server": {"endpoints": [{"scheme": "http", "host": "127.0.0.1", "port": 9080}]},
            "auth": {"type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1", "config": {}},
            "headers": {"request": {"passthrough": "all"}},
            "plugins": {"type": "transform_plugin_chain", "items": [
                {"plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1", "config": {}}
            ]},
            "rate_limit": {"sustained": {"rate": 10}, "response_headers": true},
            "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"], "allowed_methods": ["GET"]},
        }))
        .expect("full upstream input deserializes");
        assert_eq!(v.alias.as_deref(), Some("g"));
        assert_eq!(v.server.endpoints.len(), 1);
        let auth = v.auth.expect("auth block");
        assert_eq!(auth.auth_type, "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
        assert_eq!(v.plugins.items.len(), 1);
        assert_eq!(v.rate_limit.as_ref().unwrap().sustained.rate, 10);
        assert!(v.cors.enabled);

        // Missing required fields are rejected.
        assert!(serde_json::from_value::<UpstreamInput>(serde_json::json!({})).is_err());
        assert!(serde_json::from_value::<UpstreamInput>(serde_json::json!({
            "server": {"endpoints": [{"host": "a.com"}]}
        }))
        .is_err());
    }

    #[test]
    fn route_input_contract() {
        let v: RouteInput = serde_json::from_value(serde_json::json!({
            "upstream_id": uuid(),
            "match": {"http": {"methods": ["GET", "POST"], "path": "/api"}},
            "plugins": {"items": []},
        }))
        .expect("route input deserializes");
        assert!(matches!(v.match_config, MatchConfig::Http(_)));
        // `match` is the wire key, `match_config` the field name.
        assert!(serde_json::from_value::<RouteInput>(
            serde_json::json!({"upstream_id": uuid(), "match_config": {"http": {"methods": ["GET"], "path": "/x"}}})
        )
        .is_err());
    }

    #[test]
    fn plugin_input_contract() {
        let v: PluginInput = serde_json::from_value(serde_json::json!({
            "name": "p",
            "type": "guard",
            "phases": ["on_request"],
            "source_code": "def handler(ctx): pass",
        }))
        .unwrap();
        assert_eq!(v.plugin_type, crate::domain::model::PluginKind::Guard);
        assert_eq!(v.source_code.as_deref(), Some("def handler(ctx): pass"));
    }

    #[test]
    fn documents_serialize_with_envelope() {
        let up: UpstreamDocument = (&upstream()).into();
        let json = serde_json::to_value(&up).unwrap();
        assert_eq!(json["alias"], "api.openai.com");
        assert_eq!(json["protocol"], "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");

        let rt: RouteDocument = (&route()).into();
        let json = serde_json::to_value(&rt).unwrap();
        assert!(json.get("match").is_some());
        assert_eq!(json["match"]["http"]["methods"][0], "GET");

        let list = UpstreamListResponse {
            items: vec![upstream_summary()],
            total: 1,
        };
        let json = serde_json::to_value(&list).unwrap();
        let items = json["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(json["total"], 1);
    }
}
