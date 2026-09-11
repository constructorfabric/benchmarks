//! Wire DTOs for the management API.
//!
//! The shapes mirror `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` exactly — in particular `tenant_id` is
//! absent from responses, because the upstream schema is
//! `additionalProperties: false` and tenancy is implied by the caller's
//! token.
//!
//! Nested configuration blocks are typed as JSON objects on the wire and
//! projected into the domain model here, so a malformed block yields a
//! `400 ValidationError` naming the field rather than an opaque deserializer
//! message from the extractor.

use serde_json::Value;
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::gts_helpers;
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, Plugin, PluginKind, PluginPhase,
    PluginsConfig, RateLimitConfig, Route, ServerConfig, Upstream,
};
use crate::domain::services::management::{PluginSpec, RouteSpec, UpstreamSpec};

/// Project a JSON block into a typed configuration, naming the field on
/// failure.
fn project<T: serde::de::DeserializeOwned>(field: &str, value: Value) -> DomainResult<T> {
    serde_json::from_value(value)
        .map_err(|err| DomainError::validation(format!("invalid '{field}': {err}")))
}

fn project_opt<T: serde::de::DeserializeOwned>(
    field: &str,
    value: Option<Value>,
) -> DomainResult<Option<T>> {
    value.map(|v| project(field, v)).transpose()
}

fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// Upstream
// ---------------------------------------------------------------------------

/// Request body for `POST`/`PUT /oagw/v1/upstreams`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequestDto {
    /// Explicit alias. Only accepted for IP-based or otherwise non-derivable
    /// endpoint pools; for hostname pools it must equal the derived value.
    #[serde(default)]
    pub alias: Option<String>,
    /// Whether proxy traffic is accepted. Defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Discovery tags (`^[a-z0-9_-]+$`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool: `{ "endpoints": [ { "scheme", "host", "port" } ] }`.
    pub server: Value,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Outbound authentication: `{ "type", "sharing", "config" }`.
    #[serde(default)]
    pub auth: Option<Value>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<Value>,
    /// Plugin chain: `{ "sharing", "items" }`.
    #[serde(default)]
    pub plugins: Option<Value>,
    /// Rate limit.
    #[serde(default)]
    pub rate_limit: Option<Value>,
    /// CORS policy.
    #[serde(default)]
    pub cors: Option<Value>,
}

impl UpstreamRequestDto {
    /// Project the request into the Control Plane's write payload.
    ///
    /// # Errors
    ///
    /// `400` naming the field that failed to project.
    pub fn into_spec(self) -> DomainResult<UpstreamSpec> {
        Ok(UpstreamSpec {
            alias: self.alias,
            enabled: self.enabled,
            tags: self.tags,
            server: project::<ServerConfig>("server", self.server)?,
            protocol: self.protocol,
            auth: project_opt::<AuthConfig>("auth", self.auth)?,
            headers: project_opt::<HeadersConfig>("headers", self.headers)?,
            plugins: project_opt::<PluginsConfig>("plugins", self.plugins)?,
            rate_limit: project_opt::<RateLimitConfig>("rate_limit", self.rate_limit)?,
            cors: project_opt::<CorsConfig>("cors", self.cors)?,
        })
    }
}

/// Response body for the upstream endpoints.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Whether proxy traffic is accepted.
    pub enabled: bool,
    /// Routing key used in the proxy URL.
    pub alias: String,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: Value,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Outbound authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<Value>,
    /// Header transformation rules.
    pub headers: Value,
    /// Plugin chain.
    pub plugins: Value,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<Value>,
    /// CORS policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Value>,
}

impl From<&Upstream> for UpstreamResponseDto {
    fn from(upstream: &Upstream) -> Self {
        Self {
            id: upstream.id,
            enabled: upstream.enabled,
            alias: upstream.alias.clone(),
            tags: upstream.tags.clone(),
            server: encode(&upstream.server),
            protocol: upstream.protocol.clone(),
            auth: upstream.auth.as_ref().map(encode),
            headers: encode(&upstream.headers),
            plugins: encode(&upstream.plugins),
            rate_limit: upstream.rate_limit.as_ref().map(encode),
            cors: upstream.cors.as_ref().map(encode),
        }
    }
}

// ---------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------

/// Request body for `POST`/`PUT /oagw/v1/routes`.
///
/// `upstream_id` is immutable: it is required on create and ignored on
/// replace.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequestDto {
    /// Owning upstream (UUID, or the anonymous GTS identifier).
    #[serde(default)]
    pub upstream_id: Option<String>,
    /// Whether the route participates in matching. Defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Tie-breaker for equally specific paths. Defaults to `0`.
    #[serde(default)]
    pub priority: Option<i32>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Match rules; exactly one of `http` or `grpc`.
    #[serde(rename = "match")]
    pub match_config: Value,
    /// Plugin chain appended after the upstream's.
    #[serde(default)]
    pub plugins: Option<Value>,
    /// Rate limit.
    #[serde(default)]
    pub rate_limit: Option<Value>,
    /// CORS policy.
    #[serde(default)]
    pub cors: Option<Value>,
}

impl RouteRequestDto {
    /// Project the request into the Control Plane's write payload.
    ///
    /// # Errors
    ///
    /// `400` naming the field that failed to project, or an unparseable
    /// `upstream_id`.
    pub fn into_spec(self) -> DomainResult<RouteSpec> {
        let upstream_id = self
            .upstream_id
            .as_deref()
            .map(|raw| {
                gts_helpers::parse_resource_id(raw, gts_helpers::UPSTREAM_TYPE).ok_or_else(|| {
                    DomainError::validation(format!(
                        "'upstream_id' must be a UUID or a '{}{{uuid}}' identifier, got '{raw}'",
                        gts_helpers::UPSTREAM_TYPE
                    ))
                })
            })
            .transpose()?;

        Ok(RouteSpec {
            upstream_id,
            enabled: self.enabled,
            priority: self.priority,
            tags: self.tags,
            match_config: project::<MatchConfig>("match", self.match_config)?,
            plugins: project_opt::<PluginsConfig>("plugins", self.plugins)?,
            rate_limit: project_opt::<RateLimitConfig>("rate_limit", self.rate_limit)?,
            cors: project_opt::<CorsConfig>("cors", self.cors)?,
        })
    }
}

/// Response body for the route endpoints.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Tie-breaker for equally specific paths.
    pub priority: i32,
    /// Discovery tags.
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub match_config: Value,
    /// Plugin chain.
    pub plugins: Value,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<Value>,
    /// CORS policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Value>,
}

impl From<&Route> for RouteResponseDto {
    fn from(route: &Route) -> Self {
        Self {
            id: route.id,
            upstream_id: route.upstream_id,
            enabled: route.enabled,
            priority: route.priority,
            tags: route.tags.clone(),
            match_config: encode(&route.match_config),
            plugins: encode(&route.plugins),
            rate_limit: route.rate_limit.as_ref().map(encode),
            cors: route.cors.as_ref().map(encode),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// Request body for `POST /oagw/v1/plugins`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequestDto {
    /// Unique-per-tenant name.
    pub name: String,
    /// Free-form description.
    #[serde(default)]
    pub description: Option<String>,
    /// `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Declared lifecycle phases (`on_request`, `on_response`, `on_error`).
    #[serde(default)]
    pub phases: Vec<String>,
    /// JSON Schema constraining binding configuration.
    #[serde(default)]
    pub config_schema: Option<Value>,
    /// Plugin body.
    pub source_code: String,
}

impl PluginRequestDto {
    /// Project the request into the Control Plane's write payload.
    ///
    /// # Errors
    ///
    /// `400` on an unknown plugin type or phase.
    pub fn into_spec(self) -> DomainResult<PluginSpec> {
        let plugin_type = match self.plugin_type.as_str() {
            "auth" => PluginKind::Auth,
            "guard" => PluginKind::Guard,
            "transform" => PluginKind::Transform,
            other => {
                return Err(DomainError::validation(format!(
                    "'plugin_type' must be 'auth', 'guard' or 'transform', got '{other}'"
                )));
            }
        };
        let phases = self
            .phases
            .iter()
            .map(|phase| match phase.as_str() {
                "on_request" => Ok(PluginPhase::OnRequest),
                "on_response" => Ok(PluginPhase::OnResponse),
                "on_error" => Ok(PluginPhase::OnError),
                other => Err(DomainError::validation(format!(
                    "'phases' entries must be 'on_request', 'on_response' or 'on_error', \
                     got '{other}'"
                ))),
            })
            .collect::<DomainResult<Vec<_>>>()?;

        Ok(PluginSpec {
            name: self.name,
            description: self.description,
            plugin_type,
            phases,
            config_schema: self.config_schema.unwrap_or_else(|| Value::Object(
                serde_json::Map::new(),
            )),
            source_code: self.source_code,
        })
    }
}

/// Response body for the plugin endpoints.
///
/// `source_code` is deliberately absent — it is served separately by
/// `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginResponseDto {
    /// Anonymous GTS identifier of the plugin.
    pub id: String,
    /// Bare UUID, for callers that prefer it.
    pub uuid: Uuid,
    /// Unique-per-tenant name.
    pub name: String,
    /// Free-form description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Declared lifecycle phases.
    pub phases: Vec<String>,
    /// JSON Schema constraining binding configuration.
    pub config_schema: Value,
}

impl From<&Plugin> for PluginResponseDto {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: plugin.gts_id(),
            uuid: plugin.id,
            name: plugin.name.clone(),
            description: plugin.description.clone(),
            plugin_type: plugin.plugin_type.as_str().to_owned(),
            phases: plugin
                .phases
                .iter()
                .map(|phase| {
                    match phase {
                        PluginPhase::OnRequest => "on_request",
                        PluginPhase::OnResponse => "on_response",
                        PluginPhase::OnError => "on_error",
                    }
                    .to_owned()
                })
                .collect(),
            config_schema: plugin.config_schema.clone(),
        }
    }
}

/// Response body for `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// Anonymous GTS identifier of the plugin.
    pub id: String,
    /// Plugin body, verbatim.
    pub source_code: String,
}

impl From<&Plugin> for PluginSourceDto {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: plugin.gts_id(),
            source_code: plugin.source_code.clone(),
        }
    }
}

/// Envelope for the list endpoints.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ListResponseDto {
    /// Matching items, after `$filter`, `$orderby`, `$skip`, `$top` and
    /// `$select` have been applied.
    pub items: Vec<Value>,
    /// Number of items in `items`.
    pub count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn upstream_request_projects_the_documented_shape() {
        let dto: UpstreamRequestDto = serde_json::from_value(json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com", "port": 443 } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
            "auth": { "type": gts_helpers::APIKEY_AUTH_PLUGIN_ID, "config": { "secret_ref": "cred://k" } }
        }))
        .expect("deserializes");
        let spec = dto.into_spec().expect("projects");
        assert_eq!(spec.server.endpoints.len(), 1);
        assert_eq!(
            spec.auth.and_then(|a| a.plugin_ref).as_deref(),
            Some(gts_helpers::APIKEY_AUTH_PLUGIN_ID)
        );
    }

    #[test]
    fn a_plaintext_endpoint_projects_cleanly() {
        let dto: UpstreamRequestDto = serde_json::from_value(json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 80 } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
            "alias": "local-mock"
        }))
        .expect("http is a legal scheme");
        let spec = dto.into_spec().expect("projects");
        assert_eq!(
            spec.server.endpoints[0].scheme,
            crate::domain::model::Scheme::Http
        );
    }

    #[test]
    fn a_malformed_nested_block_names_the_field() {
        let dto: UpstreamRequestDto = serde_json::from_value(json!({
            "server": { "endpoints": [ { "scheme": "carrier-pigeon", "host": "h" } ] },
            "protocol": gts_helpers::PROTOCOL_HTTP
        }))
        .expect("deserializes");
        let err = dto.into_spec().expect_err("bad scheme");
        assert_eq!(err.status(), 400);
        assert!(err.detail().contains("server"));
    }

    #[test]
    fn unknown_top_level_fields_are_rejected() {
        let result: Result<UpstreamRequestDto, _> = serde_json::from_value(json!({
            "server": { "endpoints": [] },
            "protocol": gts_helpers::PROTOCOL_HTTP,
            "surprise": true
        }));
        assert!(result.is_err());
    }

    #[test]
    fn route_request_accepts_both_upstream_id_spellings() {
        let id = Uuid::new_v4();
        for raw in [
            id.to_string(),
            gts_helpers::anonymous_id(gts_helpers::UPSTREAM_TYPE, id),
        ] {
            let dto: RouteRequestDto = serde_json::from_value(json!({
                "upstream_id": raw,
                "match": { "http": { "methods": ["GET"], "path": "/v1/models" } }
            }))
            .expect("deserializes");
            assert_eq!(dto.into_spec().expect("projects").upstream_id, Some(id));
        }
    }

    #[test]
    fn upstream_response_omits_tenant_id() {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "api.openai.com".to_owned(),
            enabled: true,
            tags: vec!["llm".to_owned()],
            server: ServerConfig {
                endpoints: vec![crate::domain::model::Endpoint {
                    scheme: crate::domain::model::Scheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: Some(443),
                }],
            },
            protocol: gts_helpers::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        };
        let body = serde_json::to_value(UpstreamResponseDto::from(&upstream)).expect("serializes");
        assert!(body.get("tenant_id").is_none());
        assert_eq!(body["alias"], json!("api.openai.com"));
        assert!(body.get("auth").is_none(), "absent auth is omitted");
    }

    #[test]
    fn plugin_type_and_phase_names_are_validated() {
        let dto: PluginRequestDto = serde_json::from_value(json!({
            "name": "x",
            "plugin_type": "wizard",
            "source_code": "pass"
        }))
        .expect("deserializes");
        assert!(dto.into_spec().is_err());

        let dto: PluginRequestDto = serde_json::from_value(json!({
            "name": "x",
            "plugin_type": "guard",
            "phases": ["on_tuesday"],
            "source_code": "pass"
        }))
        .expect("deserializes");
        assert!(dto.into_spec().is_err());
    }
}
