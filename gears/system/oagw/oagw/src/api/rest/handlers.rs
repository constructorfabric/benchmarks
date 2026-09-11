//! Axum handlers.
//!
//! Management operations go straight to the Control Plane; proxy operations
//! go straight to the Data Plane (ADR-0001, path-based routing).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Extension, Path, Query, Request};
use axum::response::{IntoResponse, Response};
use http::{StatusCode, header};
use serde_json::Value;
use toolkit::Page;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{
    PluginDto, PluginSourceDto, RouteDto, UpstreamDto, parse_plugin_id, parse_route_id,
    parse_upstream_id,
};
use crate::api::rest::query::ListQuery;
use crate::domain::error::OagwError;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::service::{DataPlaneService, ProxyRequest};

/// Path prefix the proxy endpoint is mounted under, gear-relative.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// Result alias for management handlers.
pub type ApiResult = Result<Response, OagwError>;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
///
/// # Errors
///
/// `400` validation, `403` tenantless caller, `409` alias conflict.
pub async fn create_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: http::Uri,
    Json(body): Json<UpstreamDto>,
) -> ApiResult {
    let created = cp.create_upstream(&ctx, body.into_input()).await?;
    let dto = UpstreamDto::from_domain(&created);
    let location = child_location(uri.path(), dto.id.as_deref());
    Ok((StatusCode::CREATED, location, Json(dto)).into_response())
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
///
/// `400` malformed query option, `403` tenantless caller.
pub async fn list_upstreams(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    let query = ListQuery::parse(&params)?;
    let items = cp.list_upstreams(&ctx).await?;
    let serialized = serialize_all(items.iter().map(UpstreamDto::from_domain))?;
    Ok(Json(query.apply(serialized)).into_response())
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible to the caller.
pub async fn get_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    let upstream = cp.get_upstream(&ctx, parse_upstream_id(&id)?).await?;
    Ok(Json(UpstreamDto::from_domain(&upstream)).into_response())
}

/// `PUT /oagw/v1/upstreams/{id}` — full replacement; omitted optional blocks
/// are cleared.
///
/// # Errors
///
/// `400` validation, `403` tenantless caller, `404` not visible, `409`
/// alias conflict.
pub async fn replace_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<UpstreamDto>,
) -> ApiResult {
    let replaced = cp
        .replace_upstream(&ctx, parse_upstream_id(&id)?, body.into_input())
        .await?;
    Ok(Json(UpstreamDto::from_domain(&replaced)).into_response())
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible.
pub async fn delete_upstream(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(dp): Extension<Arc<DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    let uuid = parse_upstream_id(&id)?;
    let routes = cp.route_repo().list_by_upstream(uuid).await;
    cp.delete_upstream(&ctx, uuid).await?;
    // Drop the rate limit counters and breaker for the deleted resource, so a
    // re-created id never inherits a stale balance.
    dp.limiter()
        .forget(crate::infra::ratelimit::RateResource::Upstream(uuid));
    for route in routes {
        dp.limiter()
            .forget(crate::infra::ratelimit::RateResource::Route(route.id));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
///
/// # Errors
///
/// `400` validation or unknown upstream, `403` tenantless caller, `409`
/// match-rule collision.
pub async fn create_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: http::Uri,
    Json(body): Json<RouteDto>,
) -> ApiResult {
    let created = cp.create_route(&ctx, body.into_input()?).await?;
    let dto = RouteDto::from_domain(&created);
    let location = child_location(uri.path(), dto.id.as_deref());
    Ok((StatusCode::CREATED, location, Json(dto)).into_response())
}

/// `GET /oagw/v1/routes`
///
/// # Errors
///
/// `400` malformed query option, `403` tenantless caller.
pub async fn list_routes(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    let query = ListQuery::parse(&params)?;
    let items = cp.list_routes(&ctx).await?;
    let serialized = serialize_all(items.iter().map(RouteDto::from_domain))?;
    Ok(Json(query.apply(serialized)).into_response())
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible.
pub async fn get_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    let route = cp.get_route(&ctx, parse_route_id(&id)?).await?;
    Ok(Json(RouteDto::from_domain(&route)).into_response())
}

/// `PUT /oagw/v1/routes/{id}` — full replacement; `upstream_id` is immutable.
///
/// # Errors
///
/// `400` validation, `403` tenantless caller, `404` not visible, `409`
/// match-rule collision.
pub async fn replace_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<RouteDto>,
) -> ApiResult {
    let replaced = cp
        .replace_route(&ctx, parse_route_id(&id)?, body.into_input()?)
        .await?;
    Ok(Json(RouteDto::from_domain(&replaced)).into_response())
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible.
pub async fn delete_route(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(dp): Extension<Arc<DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    let uuid = parse_route_id(&id)?;
    cp.delete_route(&ctx, uuid).await?;
    dp.limiter()
        .forget(crate::infra::ratelimit::RateResource::Route(uuid));
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
///
/// # Errors
///
/// `400` validation, `403` tenantless caller, `409` duplicate name.
pub async fn create_plugin(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    uri: http::Uri,
    Json(body): Json<PluginDto>,
) -> ApiResult {
    let created = cp.create_plugin(&ctx, body.into_input()).await?;
    let dto = PluginDto::from_domain(&created);
    let location = child_location(uri.path(), dto.id.as_deref());
    Ok((StatusCode::CREATED, location, Json(dto)).into_response())
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
///
/// `400` malformed query option, `403` tenantless caller.
pub async fn list_plugins(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<HashMap<String, String>>,
) -> ApiResult {
    let query = ListQuery::parse(&params)?;
    let items = cp.list_plugins(&ctx).await?;
    let serialized = serialize_all(items.iter().map(PluginDto::from_domain))?;
    Ok(Json(query.apply(serialized)).into_response())
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible.
pub async fn get_plugin(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    let plugin = cp.get_plugin(&ctx, parse_plugin_id(&id)?).await?;
    Ok(Json(PluginDto::from_domain(&plugin)).into_response())
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible.
pub async fn get_plugin_source(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    let plugin = cp.get_plugin(&ctx, parse_plugin_id(&id)?).await?;
    Ok(Json(PluginSourceDto::from_domain(&plugin)).into_response())
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// `403` tenantless caller, `404` not visible, `409` still referenced.
pub async fn delete_plugin(
    Extension(cp): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult {
    cp.delete_plugin(&ctx, parse_plugin_id(&id)?).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`
///
/// The whole response — including every error — is produced by the Data
/// Plane, so `X-OAGW-Error-Source` is set exactly once and consistently.
pub async fn proxy(
    Extension(dp): Extension<Arc<DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    mut request: Request,
) -> Response {
    let path = request
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map_or_else(
            || request.uri().path().to_owned(),
            |original| original.0.path().to_owned(),
        );
    let Some((alias, suffix)) = split_proxy_path(request.uri().path()) else {
        return OagwError::route_not_found(
            "the proxy endpoint is addressed as /oagw/v1/proxy/{alias}[/{path}]",
        )
        .into_response();
    };
    let query: Vec<(String, String)> = request
        .uri()
        .query()
        .map(|raw| {
            form_urlencoded::parse(raw.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let on_upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();
    let method = request.method().clone();
    let headers = request.headers().clone();
    let body = request.into_body();

    dp.execute(ProxyRequest {
        alias,
        path_suffix: suffix,
        method,
        query,
        headers,
        body,
        on_upgrade,
        client_ip,
        security_context: ctx,
        instance: path,
        request_id,
    })
    .await
}

/// Split `{alias}` and the raw path suffix out of a proxy request path.
///
/// The suffix is kept in its wire (percent-encoded) form so it is forwarded
/// byte-for-byte.
#[must_use]
pub fn split_proxy_path(path: &str) -> Option<(String, Option<String>)> {
    let rest = path
        .strip_prefix(PROXY_PREFIX)
        .or_else(|| path.strip_prefix("/api/oagw/v1/proxy/"))?;
    if rest.is_empty() {
        return None;
    }
    match rest.split_once('/') {
        Some((alias, suffix)) if !alias.is_empty() => {
            Some((alias.to_owned(), Some(suffix.to_owned())))
        }
        Some(_) => None,
        None => Some((rest.to_owned(), None)),
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn serialize_all<T: serde::Serialize>(
    items: impl Iterator<Item = T>,
) -> Result<Vec<Value>, OagwError> {
    items
        .map(|item| {
            serde_json::to_value(item).map_err(|err| {
                OagwError::internal(format!("could not serialize the response: {err}"))
            })
        })
        .collect()
}

/// `Location` header for a freshly created resource.
fn child_location(collection_path: &str, id: Option<&str>) -> [(header::HeaderName, String); 1] {
    let path = match id {
        Some(id) => format!("{}/{id}", collection_path.trim_end_matches('/')),
        None => collection_path.to_owned(),
    };
    [(header::LOCATION, path)]
}

/// Page of serialized DTOs — the shape every list endpoint answers with.
pub type JsonPage = Page<Value>;

#[cfg(test)]
mod tests {
    use super::{PROXY_PREFIX, split_proxy_path};

    #[test]
    fn proxy_path_splitting() {
        assert_eq!(
            split_proxy_path("/oagw/v1/proxy/api.openai.com"),
            Some(("api.openai.com".to_owned(), None))
        );
        assert_eq!(
            split_proxy_path("/oagw/v1/proxy/api.openai.com/v1/chat/completions"),
            Some((
                "api.openai.com".to_owned(),
                Some("v1/chat/completions".to_owned())
            ))
        );
        assert_eq!(
            split_proxy_path("/oagw/v1/proxy/api.openai.com/"),
            Some(("api.openai.com".to_owned(), Some(String::new())))
        );
    }

    #[test]
    fn the_prefixed_form_is_also_understood() {
        assert_eq!(
            split_proxy_path("/api/oagw/v1/proxy/vendor.com/v1/x"),
            Some(("vendor.com".to_owned(), Some("v1/x".to_owned())))
        );
    }

    #[test]
    fn percent_encoding_in_the_suffix_is_preserved() {
        assert_eq!(
            split_proxy_path("/oagw/v1/proxy/api.openai.com/v1/a%20b"),
            Some(("api.openai.com".to_owned(), Some("v1/a%20b".to_owned())))
        );
    }

    #[test]
    fn non_proxy_paths_and_empty_aliases_are_refused() {
        assert_eq!(split_proxy_path("/oagw/v1/upstreams"), None);
        assert_eq!(split_proxy_path(PROXY_PREFIX), None);
        assert_eq!(split_proxy_path("/oagw/v1/proxy//v1/chat"), None);
    }
}
