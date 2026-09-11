//! Route management handlers.

use std::sync::Arc;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, RawQuery};
use axum::response::Response;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{ListParams, ListResponse, RouteDto};
use crate::api::rest::handlers::upstreams::{instance, into_json, no_content, problem};
use crate::api::rest::state::OagwState;

/// Attaches the GTS id of a route to its JSON form.
fn json_with_gts_id(route: &crate::domain::dto::Route, gts_id: &str) -> serde_json::Value {
    let mut value = serde_json::to_value(route).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = value.as_object_mut() {
        obj.insert("gts_id".to_string(), serde_json::Value::String(gts_id.to_string()));
    }
    value
}

/// Creates a route.
pub async fn create_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(request): Json<RouteDto>,
) -> Response {
    let instance_path = instance("/routes");
    match state.control_plane.create_route(ctx.subject_tenant_id(), request).await {
        Ok(route) => {
            let id = route.id.clone().unwrap_or_default();
            let gts_id = crate::domain::services::control_plane::ControlPlaneService::route_gts_id(&route);
            let mut body = json_with_gts_id(&route, &gts_id);
            if let Some(obj) = body.as_object_mut() {
                obj.insert("id".to_string(), serde_json::Value::String(id.clone()));
            }
            axum::response::Response::builder()
                .status(axum::http::StatusCode::CREATED)
                .header(axum::http::header::LOCATION, format!("/oagw/v1/routes/{id}"))
                .header("X-OAGW-Error-Source", "gateway")
                .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap_or_default()))
                .expect("static response")
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Reads a route by id.
pub async fn get_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/routes/{id}"));
    match state.control_plane.get_route(ctx.subject_tenant_id(), &id).await {
        Ok(route) => into_json(serde_json::to_value(&route).unwrap_or_default()),
        Err(err) => problem(err, &instance_path),
    }
}

/// Lists routes.
pub async fn list_routes(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    RawQuery(raw): RawQuery,
) -> Response {
    let instance_path = instance("/routes");
    let params = ListParams::from_query(&crate::api::rest::extractors::parse_query(
        raw.as_deref().unwrap_or_default(),
    ));
    match state.control_plane.list_routes(ctx.subject_tenant_id()).await {
        Ok(all) => {
            let all = crate::api::rest::dto::apply_filter(all, params.filter.as_deref(), |r| {
                r.upstream_id.clone()
            });
            let (page, total) = params.paginate(all);
            let page: Vec<serde_json::Value> = page
                .iter()
                .map(|r| {
                    let gts_id =
                        crate::domain::services::control_plane::ControlPlaneService::route_gts_id(r);
                    json_with_gts_id(r, &gts_id)
                })
                .collect();
            into_json(serde_json::to_value(ListResponse::new(page, Some(total), None)).unwrap_or_default())
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Replaces a route.
pub async fn update_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(request): Json<RouteDto>,
) -> Response {
    let instance_path = instance(&format!("/routes/{id}"));
    match state.control_plane.update_route(ctx.subject_tenant_id(), &id, request).await {
        Ok(route) => {
            let gts_id = crate::domain::services::control_plane::ControlPlaneService::route_gts_id(&route);
            into_json(json_with_gts_id(&route, &gts_id))
        }
        Err(err) => problem(err, &instance_path),
    }
}

/// Deletes a route.
pub async fn delete_route(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let instance_path = instance(&format!("/routes/{id}"));
    match state.control_plane.delete_route(ctx.subject_tenant_id(), &id).await {
        Ok(()) => no_content(),
        Err(err) => problem(err, &instance_path),
    }
}
