//! REST handlers for the OAGW management API and proxy.
//!
//! Every handler extracts `Extension<SecurityContext>` (inserted by the
//! api-gateway auth middleware) plus the shared `Extension<Arc<OagwService>>`
//! installed in [`super::routes`]. Management handlers return
//! `Result<_, OagwError>` whose RFC-9457 problem+json rendering (with the
//! OAGW GTS error type ids and `X-OAGW-Error-Source: gateway`) is produced
//! by [`crate::error::OagwError`].

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Extension, Path};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::StatusCode;
use toolkit_security::SecurityContext;

use super::dto::{
    CreatePluginRequest, ListResponse, PluginDto, PluginSourceDto, RouteDto, UpstreamDto,
    UpstreamRequest,
};
use crate::domain::model::RouteConfig;
use crate::domain::service::OagwService;
use crate::error::{ERROR_SOURCE_UPSTREAM, OagwError, preflight_response_headers};

fn tenant(ctx: &SecurityContext) -> uuid::Uuid {
    ctx.subject_tenant_id()
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Json(request): Json<UpstreamRequest>,
) -> Result<(StatusCode, Json<UpstreamDto>), OagwError> {
    let stored = service.create_upstream(tenant(&ctx), request)?;
    Ok((StatusCode::CREATED, Json(UpstreamDto::from_stored(&stored))))
}

pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
) -> Result<Json<ListResponse<UpstreamDto>>, OagwError> {
    let items = service
        .list_upstreams(tenant(&ctx))
        .iter()
        .map(UpstreamDto::from_stored)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<Json<UpstreamDto>, OagwError> {
    let stored = service.get_upstream(tenant(&ctx), &id)?;
    Ok(Json(UpstreamDto::from_stored(&stored)))
}

pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
    Json(request): Json<UpstreamRequest>,
) -> Result<Json<UpstreamDto>, OagwError> {
    let stored = service.update_upstream(tenant(&ctx), &id, request)?;
    Ok(Json(UpstreamDto::from_stored(&stored)))
}

pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    service.delete_upstream(tenant(&ctx), &id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Json(request): Json<RouteConfig>,
) -> Result<(StatusCode, Json<RouteDto>), OagwError> {
    let stored = service.create_route(tenant(&ctx), request)?;
    Ok((StatusCode::CREATED, Json(RouteDto::from_stored(&stored))))
}

pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
) -> Result<Json<ListResponse<RouteDto>>, OagwError> {
    let items = service
        .list_routes(tenant(&ctx), None)
        .iter()
        .map(RouteDto::from_stored)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<Json<RouteDto>, OagwError> {
    let stored = service.get_route(tenant(&ctx), &id)?;
    Ok(Json(RouteDto::from_stored(&stored)))
}

pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
    Json(request): Json<RouteConfig>,
) -> Result<Json<RouteDto>, OagwError> {
    let stored = service.update_route(tenant(&ctx), &id, request)?;
    Ok(Json(RouteDto::from_stored(&stored)))
}

pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    service.delete_route(tenant(&ctx), &id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Json(request): Json<CreatePluginRequest>,
) -> Result<(StatusCode, Json<PluginDto>), OagwError> {
    let stored = service.create_plugin(
        tenant(&ctx),
        request.name.unwrap_or_else(|| request.plugin_type.clone()),
        request.plugin_type,
        request.source_code,
        request.config_schema,
    )?;
    Ok((StatusCode::CREATED, Json(PluginDto::from_stored(&stored))))
}

pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
) -> Result<Json<ListResponse<PluginDto>>, OagwError> {
    let items = service
        .list_plugins(tenant(&ctx), None)
        .iter()
        .map(PluginDto::from_stored)
        .collect();
    Ok(Json(ListResponse::new(items)))
}

pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<Json<PluginDto>, OagwError> {
    let stored = service.get_plugin(tenant(&ctx), &id)?;
    Ok(Json(PluginDto::from_stored(&stored)))
}

pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    service.delete_plugin(tenant(&ctx), &id)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(id): Path<String>,
) -> Result<Json<PluginSourceDto>, OagwError> {
    let stored = service.get_plugin(tenant(&ctx), &id)?;
    Ok(Json(PluginSourceDto {
        id: stored.gts_id.clone(),
        source_code: stored.source_code,
    }))
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// Proxy handler for requests without a path suffix
/// (`/api/oagw/v1/proxy/{alias}`).
pub async fn proxy_alias_only(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path(alias): Path<String>,
    uri: Uri,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, OagwError> {
    let body = (!body.is_empty()).then_some(body);
    proxy_inner(&ctx, &service, &alias, "", &uri, method, headers, body).await
}

/// Proxy handler for requests with a path suffix
/// (`/api/oagw/v1/proxy/{alias}/{*path}`).
pub async fn proxy_with_path(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<OagwService>>,
    Path((alias, path)): Path<(String, String)>,
    uri: Uri,
    method: Method,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, OagwError> {
    let body = (!body.is_empty()).then_some(body);
    proxy_inner(&ctx, &service, &alias, &path, &uri, method, headers, body).await
}

/// Permissive CORS preflight responder (ADR-0004): echoes the preflight
/// inputs with a 204 and never resolves an upstream.
pub async fn proxy_preflight(headers: HeaderMap) -> Response {
    (StatusCode::NO_CONTENT, preflight_response_headers(&headers)).into_response()
}

async fn proxy_inner(
    ctx: &SecurityContext,
    service: &Arc<OagwService>,
    alias: &str,
    path: &str,
    uri: &Uri,
    method: Method,
    headers: HeaderMap,
    body: Option<axum::body::Bytes>,
) -> Result<Response, OagwError> {
    // Rebuild the request path: "/{suffix}" + query (if any). The route
    // match in the service compares against `match.http.path`, so the
    // suffix is the portion after the alias.
    let mut req_path = String::new();
    let path = path.trim();
    if path.is_empty() {
        req_path.push('/');
    } else {
        if !path.starts_with('/') {
            req_path.push('/');
        }
        req_path.push_str(path);
    }
    if let Some(query) = uri.query() {
        req_path.push('?');
        req_path.push_str(query);
    }

    let outcome = service
        .proxy(ctx, alias, &req_path, method, headers, body)
        .await?;

    let mut response = Response::new(Body::from(outcome.body));
    *response.status_mut() = outcome.status;
    for (name, value) in outcome.headers {
        if let Some(name) = name {
            response.headers_mut().append(name, value);
        }
    }
    // Success/upstream responses identify the source as upstream.
    if !is_gateway_error_status(outcome.status) {
        response.headers_mut().insert(
            crate::error::ERROR_SOURCE_HEADER,
            axum::http::header::HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
    }
    Ok(response)
}

fn is_gateway_error_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::PAYLOAD_TOO_LARGE
        || status.as_u16() >= 500
}
