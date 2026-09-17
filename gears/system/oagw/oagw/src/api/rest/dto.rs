//! REST DTOs for the OAGW management API.
//!
//! Request shapes mirror the wire schemas (`docs/schemas/upstream.v1.schema.json`,
//! `docs/schemas/route.v1.schema.json`) with every field tolerant on input
//! (`Option`/`default`), so schema-level violations surface as clean
//! `400 Validation Error` problem+json responses through the service
//! validator instead of serde/axum extractor rejections.
//!
//! Responses reuse the domain models directly (`Upstream` / `Route` /
//! `Plugin`), whose serde representation is identical to the wire schema.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginKind, PluginsConfig, Protocol,
    RateLimitConfig, ServerConfig,
};
use crate::domain::services::management::{PluginInput, RouteInput, UpstreamInput};

/// Create/replace body for an upstream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpstreamRequest {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool. Validated by the service (must be non-empty with a
    /// consistent scheme/port).
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub headers: HeadersConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

impl From<UpstreamRequest> for UpstreamInput {
    fn from(v: UpstreamRequest) -> Self {
        Self {
            enabled: v.enabled,
            alias: v.alias,
            tags: v.tags,
            server: v.server,
            protocol: v.protocol,
            auth: v.auth,
            headers: v.headers,
            plugins: v.plugins,
            rate_limit: v.rate_limit,
            cors: v.cors,
        }
    }
}

/// Create/replace body for a route.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouteRequest {
    /// Owning upstream (required — validated by the service when absent).
    #[serde(default)]
    pub upstream_id: Option<Uuid>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub priority: i32,
    /// Match rules: exactly one of `http` / `grpc` (wire key `match`).
    #[serde(default, rename = "match")]
    pub match_: MatchConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

impl TryFrom<RouteRequest> for RouteInput {
    type Error = crate::domain::error::DomainError;

    fn try_from(v: RouteRequest) -> Result<Self, Self::Error> {
        let upstream_id = v.upstream_id.ok_or_else(|| {
            crate::domain::error::DomainError::validation(
                "route.upstream_id is required",
            )
        })?;
        Ok(Self {
            enabled: v.enabled,
            tags: v.tags,
            upstream_id,
            priority: v.priority,
            match_: v.match_,
            plugins: v.plugins,
            rate_limit: v.rate_limit,
            cors: v.cors,
        })
    }
}

/// Create body for a custom (Starlark) plugin.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginRequest {
    /// Display name (required — validated by the service when absent).
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Plugin kind (`auth` / `guard` / `transform`).
    #[serde(default)]
    pub kind: Option<PluginKind>,
    /// JSON-Schema for bindings referencing this plugin.
    #[serde(default)]
    pub config_schema: Value,
    /// Starlark source (required).
    #[serde(default)]
    pub source_code: Option<String>,
}

impl TryFrom<PluginRequest> for PluginInput {
    type Error = crate::domain::error::DomainError;

    fn try_from(v: PluginRequest) -> Result<Self, Self::Error> {
        use crate::domain::error::DomainError as E;
        Ok(Self {
            name: v.name.ok_or_else(|| E::validation("plugin.name is required"))?,
            description: v.description,
            kind: v.kind.ok_or_else(|| E::validation("plugin.kind is required"))?,
            config_schema: v.config_schema,
            source_code: v
                .source_code
                .ok_or_else(|| E::validation("plugin.source_code is required"))?,
        })
    }
}

/// `{ "source": "..." }` payload for `GET /plugins/{id}/source`.
#[derive(Debug, Clone, Serialize)]
pub struct PluginSourceDto {
    pub source: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_request_parses_auth_type_wire_key() {
        // Wire schema uses `auth.type` (not `auth_type`).
        let req: UpstreamRequest = serde_json::from_value(serde_json::json!({
            "server": { "endpoints": [{ "scheme": "http", "host": "api.x.com", "port": 80 }] },
            "protocol": "http",
            "auth": { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1" },
        }))
        .unwrap();
        assert_eq!(
            req.auth.auth_type,
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
        );
    }

    #[test]
    fn route_request_parses_match_wire_key() {
        let req: RouteRequest = serde_json::from_value(serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "path": "/v1", "methods": ["GET"] } },
        }))
        .unwrap();
        assert!(req.match_.http.is_some());
        assert_eq!(req.match_.http.unwrap().path, "/v1");
    }

    #[test]
    fn route_request_requires_upstream_id() {
        let req: RouteRequest =
            serde_json::from_value(serde_json::json!({ "match": { "http": { "path": "/v1" } } }))
                .unwrap();
        assert!(RouteInput::try_from(req).is_err());
    }

    #[test]
    fn plugin_request_requires_kind_and_source() {
        let req: PluginRequest = serde_json::from_value(serde_json::json!({
            "name": "p",
        }))
        .unwrap();
        assert!(PluginInput::try_from(req).is_err());
    }
}
