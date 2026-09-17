//! Extractors for the OAGW REST layer.

use std::sync::Arc;

use axum::http::request::Parts;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// The tenant the request acts on, derived from the authenticated context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tenant(pub uuid::Uuid);

impl Tenant {
    /// The tenant id.
    #[must_use]
    pub fn id(self) -> uuid::Uuid {
        self.0
    }
}

impl From<SecurityContext> for Tenant {
    fn from(ctx: SecurityContext) -> Self {
        Self(ctx.subject_tenant_id())
    }
}

/// The authenticated caller's full security context, for handlers that need
/// more than the tenant id (hierarchical resolution needs it to walk the
/// tenant chain).
#[derive(Debug, Clone)]
pub struct Caller(pub SecurityContext);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Caller {
    type Rejection = crate::api::rest::error::ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<SecurityContext>() {
            Some(ctx) => Ok(Self(ctx.clone())),
            None => Err(DomainError::Validation(
                "no security context: authentication middleware did not run".into(),
            )
            .into()),
        }
    }
}

/// A resource id that accepts either a bare UUID or a full GTS instance id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceId(pub uuid::Uuid);

impl ResourceId {
    /// Parse a path segment.
    ///
    /// # Errors
    /// [`DomainError::Validation`] when the segment is neither a UUID nor a
    /// GTS instance id with a UUID tail.
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        let tail = raw.split('~').next_back().unwrap_or(raw);
        uuid::Uuid::parse_str(tail)
            .map(Self)
            .map_err(|_| DomainError::Validation(format!("'{raw}' is not a valid resource id")))
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Path extractor support: axum deserializes a single path parameter through
// `deserialize_str`, so the GTS-aware parsing in `Self::parse` is wired in here.
impl<'de> serde::Deserialize<'de> for ResourceId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = ResourceId;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a resource id (bare UUID or GTS instance id)")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                ResourceId::parse(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

impl std::str::FromStr for ResourceId {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// OData-style list query accepted by every list endpoint.
///
/// DESIGN §"List Query Parameters" spells the parameters with a `$` prefix
/// (`?$top=1`); the bare spellings are accepted as well, so an operator using
/// either convention gets the same page.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct ListParams {
    /// OData `$filter`.
    #[serde(alias = "$filter")]
    pub filter: Option<String>,
    /// OData `$search`.
    #[serde(alias = "$search")]
    pub search: Option<String>,
    /// OData `$select` (fields to return).
    #[serde(alias = "$select")]
    pub select: Option<String>,
    /// OData `$orderby` (`alias desc`, `created_at`, …).
    #[serde(alias = "$orderby")]
    pub orderby: Option<String>,
    /// OData `$top`.
    #[serde(alias = "$top")]
    pub top: Option<usize>,
    /// OData `$skip`.
    #[serde(alias = "$skip")]
    pub skip: Option<usize>,
}

impl From<ListParams> for crate::domain::repo::ListQuery {
    fn from(params: ListParams) -> Self {
        crate::domain::repo::ListQuery {
            filter: params.filter,
            search: params.search,
            top: params.top,
            skip: params.skip,
            orderby: params.orderby,
        }
    }
}

/// Client IP best-effort extraction, honouring the edge headers first.
#[must_use]
pub fn client_ip(headers: &axum::http::HeaderMap, connect_info: Option<std::net::SocketAddr>) -> Option<std::net::IpAddr> {
    for header in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = headers.get(header).and_then(|v| v.to_str().ok())
            && let Some(first) = value.split(',').next()
        {
            let candidate = first.trim();
            if let Ok(ip) = candidate.parse::<std::net::IpAddr>() {
                return Some(ip);
            }
        }
    }
    connect_info.map(|addr| addr.ip())
}

/// Handler state shared by every OAGW endpoint.
#[derive(Clone)]
pub struct ApiState {
    /// Upstream lifecycle service.
    pub upstreams: Arc<crate::domain::services::UpstreamService>,
    /// Route lifecycle service.
    pub routes: Arc<crate::domain::services::RouteService>,
    /// Plugin lifecycle service.
    pub plugins: Arc<crate::domain::services::PluginService>,
    /// Built-in plugin registry.
    pub plugin_registry: Arc<crate::domain::plugin::PluginRegistry>,
    /// Data-plane engine.
    pub proxy: Arc<crate::infra::proxy::ProxyEngine>,
    /// Gear configuration.
    pub config: Arc<crate::config::OagwConfig>,
}

impl std::fmt::Debug for ApiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiState").finish()
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Tenant {
    type Rejection = crate::api::rest::error::ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts.extensions.get::<SecurityContext>() {
            Some(ctx) => Ok(Self(ctx.subject_tenant_id())),
            None => Err(DomainError::Validation(
                "no security context: authentication middleware did not run".into(),
            )
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_ids_accept_bare_uuids_and_gts_ids() {
        let raw = "550e8400-e29b-41d4-a716-446655440000";
        assert_eq!(
            ResourceId::parse(raw).expect("bare uuid").0.to_string(),
            raw
        );
        assert_eq!(
            ResourceId::parse("gts.cf.core.oagw.upstream.v1~550e8400-e29b-41d4-a716-446655440000")
                .expect("gts id")
                .0
                .to_string(),
            raw
        );
        assert!(ResourceId::parse("not-a-uuid").is_err());
    }

    #[test]
    fn client_ip_prefers_forwarded_for() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.9, 10.0.0.1".parse().expect("header"),
        );
        assert_eq!(
            client_ip(&headers, None),
            Some(std::net::IpAddr::from([203u8, 0, 113, 9]))
        );
        assert!(client_ip(&axum::http::HeaderMap::new(), None).is_none());
    }
}
