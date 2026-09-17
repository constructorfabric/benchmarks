//! REST DTOs of the management API.
//!
//! Resource ids cross the wire as anonymous GTS identifiers
//! (`gts.cf.core.oagw.upstream.v1~{uuid}`); the gear stores bare UUIDs, so the
//! DTOs convert at the boundary. Specification bodies are flattened into the
//! request/response envelopes so the JSON shape matches
//! `docs/schemas/*.schema.json` exactly.

use uuid::Uuid;

use crate::domain::model::{Plugin, PluginSpec, Route, RouteSpec, Upstream, UpstreamSpec};
use crate::gts_helpers::{OagwResourceKind, resource_id_to_gts};

/// Envelope of a created or fetched upstream.
#[toolkit_macros::api_dto(request, response)]
pub struct UpstreamDto {
    /// GTS identifier of the upstream.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Upstream specification.
    #[serde(flatten)]
    pub spec: UpstreamSpec,
}

impl From<&Upstream> for UpstreamDto {
    fn from(upstream: &Upstream) -> Self {
        Self {
            id: resource_id_to_gts(OagwResourceKind::Upstream, upstream.id),
            tenant_id: upstream.tenant_id,
            spec: upstream.spec.clone(),
        }
    }
}

impl From<Upstream> for UpstreamDto {
    fn from(upstream: Upstream) -> Self {
        UpstreamDto::from(&upstream)
    }
}

/// Body of `POST /oagw/v1/upstreams`.
#[toolkit_macros::api_dto(request)]
pub struct CreateUpstreamRequest {
    /// Upstream specification; `alias` is optional for hostname endpoints.
    #[serde(flatten)]
    pub spec: UpstreamSpec,
}

/// Body of `PUT /oagw/v1/upstreams/{id}`.
#[toolkit_macros::api_dto(request)]
pub struct ReplaceUpstreamRequest {
    /// Full replacement specification; omitted optional fields are cleared.
    #[serde(flatten)]
    pub spec: UpstreamSpec,
}

/// Envelope of a created or fetched route.
#[toolkit_macros::api_dto(request, response)]
pub struct RouteDto {
    /// GTS identifier of the route.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// GTS identifier of the referenced upstream.
    pub upstream_id: String,
    /// Route specification.
    #[serde(flatten)]
    pub spec: RouteSpec,
}

impl RouteDto {
    /// Builds the DTO of a stored route.
    #[must_use]
    pub fn from_parts(id: Uuid, tenant_id: Uuid, upstream_id: Uuid, spec: &RouteSpec) -> Self {
        Self {
            id: resource_id_to_gts(OagwResourceKind::Route, id),
            tenant_id,
            upstream_id: resource_id_to_gts(OagwResourceKind::Upstream, upstream_id),
            spec: spec.clone(),
        }
    }
}

impl From<&Route> for RouteDto {
    fn from(route: &Route) -> Self {
        Self::from_parts(route.id, route.tenant_id, route.upstream_id, &route.spec)
    }
}

/// Body of `POST /oagw/v1/routes`; the upstream reference is mandatory.
#[toolkit_macros::api_dto(request)]
pub struct CreateRouteRequest {
    /// GTS identifier (or bare UUID) of the upstream to attach the route to.
    pub upstream_id: String,
    /// Route specification.
    #[serde(flatten)]
    pub spec: RouteSpec,
}

/// Body of `PUT /oagw/v1/routes/{id}`; `upstream_id` is immutable.
#[toolkit_macros::api_dto(request)]
pub struct ReplaceRouteRequest {
    /// Route specification.
    #[serde(flatten)]
    pub spec: RouteSpec,
}

/// Envelope of a created or fetched custom plugin.
#[toolkit_macros::api_dto(request, response)]
pub struct PluginDto {
    /// GTS identifier of the plugin.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Plugin specification.
    #[serde(flatten)]
    pub spec: PluginSpec,
}

impl From<&Plugin> for PluginDto {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: resource_id_to_gts(OagwResourceKind::Plugin, plugin.id),
            tenant_id: plugin.tenant_id,
            spec: plugin.spec.clone(),
        }
    }
}

/// Body of `POST /oagw/v1/plugins`.
#[toolkit_macros::api_dto(request)]
pub struct CreatePluginRequest {
    /// Plugin specification.
    #[serde(flatten)]
    pub spec: PluginSpec,
}

/// Body of `GET /oagw/v1/plugins/{id}/source`.
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// GTS identifier of the plugin.
    pub id: String,
    /// Plugin kind.
    pub plugin_type: String,
    /// Starlark source of the plugin.
    pub source_code: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, EndpointScheme, HttpMatch, MatchConfig, PathSuffixMode, ServerConfig,
        UpstreamSpec,
    };

    fn spec() -> UpstreamSpec {
        UpstreamSpec {
            enabled: true,
            alias: Some("api.openai.com".to_owned()),
            tags: vec!["llm".to_owned()],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: None,
                }],
            },
            ..UpstreamSpec::default()
        }
    }

    #[test]
    fn upstream_dtos_round_trip_through_json() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            spec: spec(),
        };
        let dto = UpstreamDto::from(&upstream);
        let expected_id = format!("gts.cf.core.oagw.upstream.v1~{}", upstream.id);
        assert_eq!(dto.id, expected_id);
        let json = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(json["id"], serde_json::Value::String(expected_id.clone()));
        assert_eq!(json["alias"], "api.openai.com");
        assert_eq!(json["server"]["endpoints"][0]["host"], "api.openai.com");
        let parsed: UpstreamDto = serde_json::from_value(json).expect("deserialize");
        assert_eq!(parsed.id, expected_id);
        assert_eq!(parsed.spec, upstream.spec);
    }

    #[test]
    fn create_requests_flatten_the_specification() {
        let json = serde_json::json!({
            "alias": "api.openai.com",
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.openai.com" }
            ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        });
        let request: CreateUpstreamRequest = serde_json::from_value(json).expect("parse");
        assert_eq!(request.spec.alias.as_deref(), Some("api.openai.com"));
        assert_eq!(request.spec.server.endpoints.len(), 1);

        // A body without the server block still parses: every field of the
        // flattened specification defaults.
        let minimal: CreateUpstreamRequest =
            serde_json::from_value(serde_json::json!({})).expect("defaults");
        assert!(minimal.spec.server.endpoints.is_empty());
    }

    #[test]
    fn route_dtos_carry_the_upstream_reference() {
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            spec: RouteSpec {
                tags: Vec::new(),
                match_rule: MatchConfig {
                    http: Some(HttpMatch {
                        methods: vec!["GET".to_owned()],
                        path: "/v1".to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        };
        let dto = RouteDto::from(&route);
        let json = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(
            json["upstream_id"],
            serde_json::Value::String(format!(
                "gts.cf.core.oagw.upstream.v1~{}",
                route.upstream_id
            ))
        );
        assert_eq!(json["match"]["http"]["path"], "/v1");
    }
}
