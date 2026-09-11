//! Wire DTOs for the management API (DESIGN §3.3).
//!
//! The domain model is the storage shape; these are the request/response shapes
//! with server-managed fields (`id`, `tenant_id`, `gts_id`, timestamps) kept out
//! of the create/replace payloads.

use http::HeaderValue;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, MatchConfig, PluginsConfig, RateLimitConfig, Route,
    ServerConfig, Upstream,
};

/// `POST /upstreams` and `PUT /upstreams/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamRequest {
    /// Routing key; derived from the endpoints when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Defaults to `true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerConfig,
    /// Defaults to the HTTP protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// Outbound auth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: crate::domain::model::HeadersConfig,
    /// Guard/transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl UpstreamRequest {
    /// Builds a domain upstream, leaving alias derivation to the service.
    #[must_use]
    pub fn into_domain(self, tenant_id: &str) -> Upstream {
        let id = Uuid::new_v4();
        Upstream {
            id,
            gts_id: gts::resource_id(gts::TYPE_UPSTREAM, &id),
            tenant_id: tenant_id.to_owned(),
            alias: self.alias.unwrap_or_default(),
            enabled: self.enabled.unwrap_or(true),
            tags: self.tags,
            server: self.server,
            protocol: self
                .protocol
                .unwrap_or_else(|| gts::PROTOCOL_HTTP.to_owned()),
            auth: self.auth,
            headers: self.headers,
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            cors: self.cors,
            created_at: crate::domain::model::now_rfc3339(),
            updated_at: crate::domain::model::now_rfc3339(),
        }
    }

    /// Applies a full replacement: omitted optional fields are cleared.
    pub fn apply_to(self, existing: &mut Upstream) {
        existing.alias = self.alias.unwrap_or_else(|| existing.alias.clone());
        existing.enabled = self.enabled.unwrap_or(true);
        existing.tags = self.tags;
        existing.server = self.server;
        existing.protocol = self
            .protocol
            .unwrap_or_else(|| gts::PROTOCOL_HTTP.to_owned());
        existing.auth = self.auth;
        existing.headers = self.headers;
        existing.plugins = self.plugins;
        existing.rate_limit = self.rate_limit;
        existing.cors = self.cors;
        existing.updated_at = crate::domain::model::now_rfc3339();
    }
}

/// A stored upstream as the API returns it.
pub type UpstreamResponse = Upstream;

/// `POST /routes` and `PUT /routes/{id}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteRequest {
    /// Upstream this route serves (create only; immutable afterwards).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(default)]
    pub match_config: MatchConfig,
    /// Match precedence among siblings (lower runs first on a tie).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// Guard/transform plugin bindings.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS policy, overriding the upstream's when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl RouteRequest {
    /// Builds a route for storage.
    ///
    /// # Errors
    /// Returns [`DomainError::Validation`] when `upstream_id` is missing.
    pub fn into_domain(self, tenant_id: &str) -> Result<Route, DomainError> {
        let upstream_id = self
            .upstream_id
            .ok_or_else(|| DomainError::Validation("route requires an `upstream_id`".to_owned()))?;
        let id = Uuid::new_v4();
        Ok(Route {
            id,
            gts_id: gts::resource_id(gts::TYPE_ROUTE, &id),
            tenant_id: tenant_id.to_owned(),
            upstream_id,
            enabled: self.enabled.unwrap_or(true),
            tags: self.tags,
            match_config: self.match_config,
            priority: self.priority.unwrap_or_default(),
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            cors: self.cors,
            created_at: crate::domain::model::now_rfc3339(),
            updated_at: crate::domain::model::now_rfc3339(),
        })
    }

    /// Applies a full replacement; `upstream_id` is immutable.
    pub fn apply_to(self, existing: &mut Route) {
        existing.enabled = self.enabled.unwrap_or(true);
        existing.tags = self.tags;
        existing.match_config = self.match_config;
        existing.priority = self.priority.unwrap_or_default();
        existing.plugins = self.plugins;
        existing.rate_limit = self.rate_limit;
        existing.cors = self.cors;
        existing.updated_at = crate::domain::model::now_rfc3339();
    }
}

/// A stored route as the API returns it.
pub type RouteResponse = Route;

/// `POST /plugins`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginRequest {
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// `auth`, `guard` or `transform`.
    #[serde(rename = "type", default)]
    pub plugin_type: String,
    /// JSON schema describing the plugin's config.
    #[serde(default)]
    pub config_schema: serde_json::Value,
    /// Starlark source.
    #[serde(default)]
    pub source_code: String,
}

impl PluginRequest {
    /// Builds a plugin for storage.
    #[must_use]
    pub fn into_domain(self, tenant_id: &str) -> crate::domain::model::Plugin {
        let id = Uuid::new_v4();
        crate::domain::model::Plugin {
            id,
            gts_id: gts::resource_id(gts::TYPE_PLUGIN, &id),
            tenant_id: tenant_id.to_owned(),
            name: self.name,
            plugin_type: self.plugin_type,
            config_schema: self.config_schema,
            source_code: self.source_code,
            created_at: crate::domain::model::now_rfc3339(),
        }
    }
}

/// A stored plugin as the API returns it.
pub type PluginResponse = crate::domain::model::Plugin;

/// The response body of a list endpoint.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListResponse<T> {
    /// The matching resources.
    pub items: Vec<T>,
    /// Total number of resources the filter matches.
    pub total: usize,
}

impl<T> ListResponse<T> {
    /// Wraps a page of results.
    #[must_use]
    pub fn new(items: Vec<T>, total: usize) -> Self {
        Self { items, total }
    }
}

/// The accepted create/replace upstream body, as parsed from JSON.
///
/// Kept as an alias so handler signatures read naturally.
pub type CreateUpstream = UpstreamRequest;
/// The accepted create/replace route body.
pub type CreateRoute = RouteRequest;
/// The accepted create-plugin body.
pub type CreatePlugin = PluginRequest;

/// Builds a `201` response carrying the created resource.
#[must_use]
pub fn created<T: Serialize>(value: &T) -> http::Response<axum::body::Body> {
    json_response(http::StatusCode::CREATED, value)
}

/// Builds a JSON response with the given status.
#[must_use]
pub fn json_response<T: Serialize>(
    status: http::StatusCode,
    value: &T,
) -> http::Response<axum::body::Body> {
    // Built by hand rather than through the fallible builder: a status code
    // plus one well-known header cannot fail to construct.
    let mut response = http::Response::new(axum::body::Body::from(
        serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// An empty response with the given status.
#[must_use]
pub fn empty_response(status: http::StatusCode) -> http::Response<axum::body::Body> {
    let mut response = http::Response::new(axum::body::Body::empty());
    *response.status_mut() = status;
    response
}

/// A convenience constructor used by the tests for a single-endpoint server.
#[must_use]
pub fn server_one(host: &str, port: u16, https: bool) -> ServerConfig {
    ServerConfig {
        endpoints: vec![Endpoint {
            scheme: if https {
                crate::domain::model::EndpointScheme::Https
            } else {
                crate::domain::model::EndpointScheme::Http
            },
            host: host.to_owned(),
            port: Some(port),
        }],
    }
}
