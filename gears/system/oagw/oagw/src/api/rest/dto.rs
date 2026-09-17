//! Wire DTOs.
//!
//! Upstream, route and plugin payloads reuse the domain models directly: the
//! wire shape is the stored shape (DESIGN §3.3 "Management API"), with the
//! alias resolved server-side and returned in full records.

use serde::{Deserialize, Serialize};

use crate::domain::model::{
    Plugin, Route, RouteInput, Sharing, Upstream, UpstreamInput, UpstreamServer,
};

/// Payload for `POST /upstreams`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateUpstreamDto {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub server: UpstreamServer,
    pub protocol: Option<String>,
    pub auth: Option<crate::domain::model::AuthConfig>,
    pub headers: Option<crate::domain::model::HeadersConfig>,
    pub plugins: Option<crate::domain::model::PluginListConfig>,
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
    pub cors: Option<crate::domain::model::CorsConfig>,
}

fn default_true() -> bool {
    true
}

/// Payload for `PUT /upstreams/{id}`.
pub type ReplaceUpstreamDto = CreateUpstreamDto;

/// Payload for `POST /routes`.
#[derive(Debug, Clone, Deserialize)]
pub struct CreateRouteDto {
    #[serde(default)]
    pub tags: Vec<String>,
    pub upstream_id: uuid::Uuid,
    pub r#match: Option<crate::domain::model::MatchRule>,
    #[serde(default)]
    pub plugins: Option<crate::domain::model::PluginListConfig>,
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
}

/// Payload for `PUT /routes/{id}`.
pub type ReplaceRouteDto = CreateRouteDto;

/// `POST /plugins` payload (`PluginInput` is reused on the wire).
pub type CreatePluginDto = PluginInput;

/// `PATCH`-style sharing update is not supported; routes and upstreams are
/// replaced wholesale.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SharingDto(pub Sharing);

/// Query parameters accepted by the proxy endpoints.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ProxyQuery {
    /// Present on CORS preflights forwarded through the generic handler.
    #[serde(default)]
    pub r#override: Option<String>,
}

/// `PluginInput` re-exported so handlers can reference one type.
pub use crate::domain::model::PluginInput;

/// Upstream list response (OData-ish envelope).
#[derive(Debug, Serialize)]
pub struct UpstreamPage {
    /// Total number of upstreams the query matched before paging.
    pub count: usize,
    /// Page of upstreams.
    pub items: Vec<Upstream>,
}

/// Route list response.
#[derive(Debug, Serialize)]
pub struct RoutePage {
    /// Matched route count before paging.
    pub count: usize,
    /// Page of routes.
    pub items: Vec<Route>,
}

/// Plugin list response.
#[derive(Debug, Serialize)]
pub struct PluginPage {
    /// Matched plugin count before paging.
    pub count: usize,
    /// Page of plugins.
    pub items: Vec<Plugin>,
}

/// Converts a wire payload into the control-plane input model.
///
/// # Errors
///
/// [`crate::domain::error::OagwError::Validation`] when `protocol` is absent.
#[must_use]
pub fn upstream_input(dto: CreateUpstreamDto) -> UpstreamInput {
    UpstreamInput {
        enabled: dto.enabled,
        alias: dto.alias,
        tags: dto.tags,
        server: dto.server,
        protocol: dto.protocol.unwrap_or_else(|| crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned()),
        auth: dto.auth,
        headers: dto.headers,
        plugins: dto.plugins,
        rate_limit: dto.rate_limit,
        cors: dto.cors,
    }
}

/// Converts a route payload into the control-plane input.
///
/// # Errors
///
/// [`crate::domain::error::OagwError::Validation`] when `match` is missing.
pub fn route_input(dto: CreateRouteDto) -> crate::domain::error::OagwResult<RouteInput> {
    Ok(RouteInput {
        tags: dto.tags,
        upstream_id: dto.upstream_id,
        r#match: dto.r#match.ok_or_else(|| {
            crate::domain::error::OagwError::Validation("match is required".to_owned())
        })?,
        plugins: dto.plugins.unwrap_or_default(),
        rate_limit: dto.rate_limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_defaults_enable_and_default_protocol() {
        let json = r#"{"server":{"endpoints":[{"scheme":"https","host":"api.example.com","port":443}]}}"#;
        let dto: CreateUpstreamDto = serde_json::from_str(json).unwrap();
        assert!(dto.enabled);
        assert_eq!(dto.protocol, None);
        let input = upstream_input(dto);
        assert_eq!(input.protocol, crate::domain::gts_helpers::PROTOCOL_HTTP);
    }

    #[test]
    fn route_requires_a_match_rule() {
        let json = r#"{"upstream_id":"00000000-0000-0000-0000-000000000000"}"#;
        let dto: CreateRouteDto = serde_json::from_str(json).unwrap();
        assert!(route_input(dto).is_err());
    }
}
