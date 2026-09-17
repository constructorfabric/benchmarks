//! Axum handlers for the OAGW REST surface.
//!
//! Control-plane handlers forward to the [`ControlPlaneServiceImpl`]; the
//! proxy handlers assemble a [`ProxyInput`] (client IP from the server's
//! `ConnectInfo` extension when present) and invoke the data plane.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::extract::{Path, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use toolkit_security::SecurityContext;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::services::data_plane::{DataPlaneService, ProxyInput};
use crate::domain::services::management::{
    ControlPlaneService, ControlPlaneServiceImpl,
};
use crate::infra::proxy::service::DataPlaneServiceImpl;

use super::dto::{PluginRequest, PluginSourceDto, RouteRequest, UpstreamRequest};
use super::error::OagwError;
use super::extractors::IdPath;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams` — create an upstream.
///
/// Responds `201 Created` to match the `.json_response(StatusCode::CREATED, ..)`
/// contract registered in `routes.rs` (the plain `Json` default would be 200).
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    Json(input): Json<UpstreamRequest>,
) -> Result<(StatusCode, Json<Upstream>), OagwError> {
    let upstream = svc.create_upstream(&ctx, input.into()).await?;
    Ok((StatusCode::CREATED, Json(upstream)))
}

/// `GET /oagw/v1/upstreams` — list the tenant's upstreams.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
) -> Result<Json<Vec<Upstream>>, OagwError> {
    let upstreams = svc.list_upstreams(&ctx).await?;
    Ok(Json(upstreams))
}

/// `GET /oagw/v1/upstreams/{id}` — get one upstream.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<Json<Upstream>, OagwError> {
    let upstream = svc.get_upstream(&ctx, id).await?;
    Ok(Json(upstream))
}

/// `PUT /oagw/v1/upstreams/{id}` — replace an upstream.
pub async fn update_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
    Json(input): Json<UpstreamRequest>,
) -> Result<Json<Upstream>, OagwError> {
    let upstream = svc.update_upstream(&ctx, id, input.into()).await?;
    Ok(Json(upstream))
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete an upstream (cascades routes).
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<StatusCode, OagwError> {
    svc.delete_upstream(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/upstreams/{id}/enable` — enable an upstream.
pub async fn enable_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<Json<Upstream>, OagwError> {
    let upstream = svc.set_upstream_enabled(&ctx, id, true).await?;
    Ok(Json(upstream))
}

/// `POST /oagw/v1/upstreams/{id}/disable` — disable an upstream (503 on use).
pub async fn disable_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<Json<Upstream>, OagwError> {
    let upstream = svc.set_upstream_enabled(&ctx, id, false).await?;
    Ok(Json(upstream))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes` — create a route.
///
/// Responds `201 Created` per the `routes.rs` contract.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    Json(input): Json<RouteRequest>,
) -> Result<(StatusCode, Json<Route>), OagwError> {
    let route = svc.create_route(&ctx, input.try_into()?).await?;
    Ok((StatusCode::CREATED, Json(route)))
}

/// `GET /oagw/v1/routes` — list the tenant's routes.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
) -> Result<Json<Vec<Route>>, OagwError> {
    let routes = svc.list_routes(&ctx).await?;
    Ok(Json(routes))
}

/// `GET /oagw/v1/routes/{id}` — get one route.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<Json<Route>, OagwError> {
    let route = svc.get_route(&ctx, id).await?;
    Ok(Json(route))
}

/// `PUT /oagw/v1/routes/{id}` — replace a route (upstream is immutable).
pub async fn update_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
    Json(input): Json<RouteRequest>,
) -> Result<Json<Route>, OagwError> {
    let route = svc.update_route(&ctx, id, input.try_into()?).await?;
    Ok(Json(route))
}

/// `DELETE /oagw/v1/routes/{id}` — delete a route.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<StatusCode, OagwError> {
    svc.delete_route(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Custom plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins` — create a custom plugin.
///
/// Responds `201 Created` per the `routes.rs` contract.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    Json(input): Json<PluginRequest>,
) -> Result<(StatusCode, Json<Plugin>), OagwError> {
    let plugin = svc.create_plugin(&ctx, input.try_into()?).await?;
    Ok((StatusCode::CREATED, Json(plugin)))
}

/// `GET /oagw/v1/plugins` — list the tenant's custom plugins.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
) -> Result<Json<Vec<Plugin>>, OagwError> {
    let plugins = svc.list_plugins(&ctx).await?;
    Ok(Json(plugins))
}

/// `GET /oagw/v1/plugins/{id}` — get one custom plugin.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<Json<Plugin>, OagwError> {
    let plugin = svc.get_plugin(&ctx, id).await?;
    Ok(Json(plugin))
}

/// `DELETE /oagw/v1/plugins/{id}` — delete a plugin (409 when in use).
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<StatusCode, OagwError> {
    svc.delete_plugin(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /oagw/v1/plugins/{id}/source` — fetch the Starlark source.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneServiceImpl>>,
    IdPath(id): IdPath,
) -> Result<Json<PluginSourceDto>, OagwError> {
    let source = svc.get_plugin_source(&ctx, id).await?;
    Ok(Json(PluginSourceDto { source }))
}

// ---------------------------------------------------------------------------
// Data plane
// ---------------------------------------------------------------------------

/// `{METHOD} /oagw/v1/proxy/{alias}` — proxy with no path suffix.
pub async fn proxy_no_suffix(
    Extension(ctx): Extension<SecurityContext>,
    Extension(dp): Extension<Arc<DataPlaneServiceImpl>>,
    Path(alias): Path<String>,
    request: Request,
) -> Result<Response<Body>, OagwError> {
    proxy_core(ctx, dp, alias, String::new(), request).await
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path_suffix}` — proxy with suffix.
pub async fn proxy_with_suffix(
    Extension(ctx): Extension<SecurityContext>,
    Extension(dp): Extension<Arc<DataPlaneServiceImpl>>,
    Path((alias, path_suffix)): Path<(String, String)>,
    request: Request,
) -> Result<Response<Body>, OagwError> {
    proxy_core(ctx, dp, alias, path_suffix, request).await
}

/// Shared proxy pipeline entry (alias resolution + routing happen inside
/// the data plane).
async fn proxy_core(
    ctx: SecurityContext,
    dp: Arc<DataPlaneServiceImpl>,
    alias: String,
    path_suffix: String,
    request: Request,
) -> Result<Response<Body>, OagwError> {
    // The server runs `into_make_service_with_connect_info`; the peer
    // address lands in request extensions (absent for in-process tests).
    let client_ip = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());

    let input = ProxyInput {
        alias,
        path_suffix,
        client_ip,
        request,
    };
    dp.execute_proxy(ctx, input).await.map_err(OagwError::from)
}

/// CORS preflight: `OPTIONS /oagw/v1/proxy/{alias}...` → permissive 204
/// (ADR 0004 — answered at the handler level).
pub async fn proxy_preflight(request: Request) -> Response<Body> {
    use crate::infra::proxy::cors::preflight_response;
    let resp = preflight_response(request.headers());
    resp.map(|_| Body::empty()).into_response()
}
