//! Wire DTOs for the management API.
//!
//! The DTOs mirror `docs/schemas/{upstream,route}.v1.schema.json` with the
//! documented wire overrides applied (`http` is a legal endpoint scheme).
//! `id`, `tenant_id` and `created_at`/`updated_at` are server-owned and are
//! therefore absent from every request body.

use std::collections::BTreeSet;

use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, HttpMatch, PluginsConfig, Protocol,
    RateLimitConfig, Route, RouteMatch, Upstream,
};

/// Endpoint description (`scheme`, `host`, optional `port`).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct EndpointDto {
    /// Wire scheme; `http`, `https`, `wss`, `wt` and `grpc` are accepted.
    pub scheme: String,
    /// Hostname or IP literal.
    pub host: String,
    /// Explicit port; defaults to the scheme's standard port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl EndpointDto {
    /// Converts to the domain endpoint, normalizing the scheme text.
    ///
    /// # Errors
    ///
    /// Returns a 400 problem when the scheme is unknown.
    pub fn into_endpoint(self) -> Result<Endpoint, crate::domain::error::DomainError> {
        let scheme = serde_json::from_value::<crate::domain::model::EndpointScheme>(
            serde_json::Value::String(self.scheme.clone()),
        )
        .map_err(|_| {
            crate::domain::error::DomainError::new(
                crate::domain::error::ErrorKind::Validation,
                format!("unknown endpoint scheme '{scheme}'", scheme = self.scheme),
            )
        })?;
        Ok(Endpoint {
            scheme,
            host: self.host,
            port: self.port,
        })
    }
}

/// Create/replace upstream request body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct UpsertUpstreamDto {
    /// Explicit alias; required when the endpoint pool is not derivable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic. Defaults to `true`.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tags: BTreeSet<String>,
    /// Endpoint pool.
    pub server: ServerDto,
    /// Upstream protocol. Defaults to HTTP.
    #[serde(default)]
    pub protocol: Protocol,
    /// Outbound auth configuration.
    #[serde(default)]
    pub auth: AuthConfig,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Rate limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: CorsConfig,
}

impl Default for UpsertUpstreamDto {
    fn default() -> Self {
        Self {
            alias: None,
            enabled: true,
            tags: BTreeSet::new(),
            server: ServerDto::default(),
            protocol: Protocol::default(),
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: CorsConfig::default(),
        }
    }
}

/// Endpoint pool.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct ServerDto {
    /// Ordered endpoint list.
    pub endpoints: Vec<EndpointDto>,
}

/// Upstream resource as returned by the management API.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Normalized routing key.
    pub alias: String,
    /// Flat tags.
    pub tags: BTreeSet<String>,
    /// Endpoint pool.
    pub server: ServerDto,
    /// Upstream protocol GTS identifier.
    pub protocol: String,
    /// Outbound auth configuration.
    pub auth: AuthConfig,
    /// Header transformation rules.
    pub headers: HeadersConfig,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate limit configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: CorsConfig,
    /// Creation instant (epoch millis).
    pub created_at: i64,
    /// Last modification instant (epoch millis).
    pub updated_at: i64,
}

impl From<Upstream> for UpstreamDto {
    fn from(value: Upstream) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id,
            enabled: value.enabled,
            alias: value.alias,
            tags: value.tags,
            server: ServerDto {
                endpoints: value
                    .server
                    .endpoints
                    .iter()
                    .map(|endpoint| EndpointDto {
                        scheme: endpoint.scheme.wire_name().to_owned(),
                        host: endpoint.host.clone(),
                        port: endpoint.port,
                    })
                    .collect(),
            },
            protocol: value.protocol.gts_id().to_owned(),
            auth: value.auth,
            headers: value.headers,
            plugins: value.plugins,
            rate_limit: value.rate_limit,
            cors: value.cors,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Create/replace route request body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct UpsertRouteDto {
    /// Owning upstream. Ignored on replace (the reference is immutable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<uuid::Uuid>,
    /// Whether the route participates in matching. Defaults to `true`.
    #[serde(default = "crate::domain::model::default_true")]
    pub enabled: bool,
    /// Flat tags.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tags: BTreeSet<String>,
    /// Matching rules.
    #[serde(rename = "match")]
    pub route_match: RouteMatch,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginsConfig,
    /// Rate limit configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

/// Route resource as returned by the management API.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Owning upstream (immutable).
    pub upstream_id: uuid::Uuid,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat tags.
    pub tags: BTreeSet<String>,
    /// Matching rules.
    #[serde(rename = "match")]
    pub route_match: RouteMatch,
    /// Plugin chain.
    pub plugins: PluginsConfig,
    /// Rate limit configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// Creation instant (epoch millis).
    pub created_at: i64,
    /// Last modification instant (epoch millis).
    pub updated_at: i64,
}

impl From<Route> for RouteDto {
    fn from(value: Route) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id,
            upstream_id: value.upstream_id,
            enabled: value.enabled,
            tags: value.tags,
            route_match: value.route_match,
            plugins: value.plugins,
            rate_limit: value.rate_limit,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Create plugin request body. Plugins are immutable (no replace).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreatePluginDto {
    /// Human readable name.
    pub name: String,
    /// Plugin kind (`auth`, `guard` or `transform`).
    pub plugin_type: String,
    /// Starlark source. Never echoed into an error response.
    #[serde(default)]
    pub source: String,
}

/// Plugin resource as returned by the management API.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Server-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Human readable name.
    pub name: String,
    /// Plugin kind.
    pub plugin_type: String,
    /// Unlinked-since instant for garbage collection (epoch millis).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<i64>,
    /// Creation instant (epoch millis).
    pub created_at: i64,
}

impl From<crate::domain::model::Plugin> for PluginDto {
    fn from(value: crate::domain::model::Plugin) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id,
            name: value.name,
            plugin_type: value.plugin_type,
            gc_eligible_at: value.gc_eligible_at,
            created_at: value.created_at,
        }
    }
}

/// Builds a [`Route`] from a create/replace body.
#[must_use]
pub fn route_from_dto(dto: &UpsertRouteDto) -> Route {
    Route {
        id: uuid::Uuid::nil(),
        tenant_id: uuid::Uuid::nil(),
        upstream_id: dto.upstream_id.unwrap_or_else(uuid::Uuid::nil),
        enabled: dto.enabled,
        tags: dto.tags.clone(),
        route_match: dto.route_match.clone(),
        plugins: dto.plugins.clone(),
        rate_limit: dto.rate_limit.clone(),
        created_at: 0,
        updated_at: 0,
    }
}

/// Builds an [`Upstream`] skeleton from a create/replace body.
#[must_use]
pub fn upstream_from_dto(dto: &UpsertUpstreamDto) -> Upstream {
    Upstream {
        id: uuid::Uuid::nil(),
        tenant_id: uuid::Uuid::nil(),
        enabled: dto.enabled,
        alias: String::new(),
        tags: dto.tags.clone(),
        server: crate::domain::model::ServerConfig {
            endpoints: Vec::new(),
        },
        protocol: dto.protocol,
        auth: dto.auth.clone(),
        headers: dto.headers.clone(),
        plugins: dto.plugins.clone(),
        rate_limit: dto.rate_limit.clone(),
        cors: dto.cors.clone(),
        created_at: 0,
        updated_at: 0,
    }
}

/// Builds the HTTP match used by the happy-path tests.
#[must_use]
pub fn simple_match(methods: &[&str], path: &str) -> RouteMatch {
    RouteMatch {
        http: Some(HttpMatch {
            methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn endpoint_dto_rejects_unknown_scheme() {
        let err = EndpointDto {
            scheme: "ftp".to_owned(),
            host: "api.example".to_owned(),
            port: None,
        }
        .into_endpoint()
        .expect_err("unknown scheme");
        assert_eq!(err.kind, crate::domain::error::ErrorKind::Validation);
        assert!(format!("{err}").contains("unknown endpoint scheme 'ftp'"));
    }

    #[test]
    fn endpoint_dto_accepts_http() {
        let endpoint = EndpointDto {
            scheme: "http".to_owned(),
            host: "upstream".to_owned(),
            port: Some(8080),
        }
        .into_endpoint()
        .expect("http is a legal scheme");
        assert!(endpoint.scheme.is_plaintext());
        assert_eq!(endpoint.effective_port(), 8080);
    }

    #[test]
    fn upstream_dto_omits_absent_optionals() {
        let dto = UpsertUpstreamDto {
            server: ServerDto {
                endpoints: vec![EndpointDto {
                    scheme: "https".to_owned(),
                    host: "api.example".to_owned(),
                    port: None,
                }],
            },
            ..UpsertUpstreamDto::default()
        };
        let text = serde_json::to_string(&dto).expect("serialize");
        assert!(!text.contains("\"rate_limit\""));
        assert!(!text.contains("\"alias\""));
        assert!(text.contains("\"enabled\":true"));
    }

    #[test]
    fn route_dto_uses_match_as_key() {
        let dto: UpsertRouteDto = serde_json::from_str(
            r#"{"upstream_id":"00000000-0000-0000-0000-000000000001","match":{"http":{"methods":["GET"],"path":"/v1"}}}"#,
        )
        .expect("deserialize");
        assert_eq!(dto.route_match.http.as_ref().expect("http").path, "/v1");
        let route = route_from_dto(&dto);
        assert_eq!(route.upstream_id, uuid::Uuid::from_u128(1));
    }
}
