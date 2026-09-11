//! Management REST handlers (DESIGN §3.3 "Management API").
//!
//! Every handler is a thin adapter: extract the tenant from the security
//! context, call the [`ControlPlaneService`], and render either the JSON
//! resource or the RFC 9457 problem for the [`DomainError`] it returned.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path};
use http::StatusCode;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    self, CreatePlugin, CreateRoute, CreateUpstream, ListResponse, PluginResponse, RouteResponse,
    UpstreamResponse,
};
use crate::api::rest::error;
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::service::ProxyBody;

type Response = http::Response<ProxyBody>;
type Service = Arc<ControlPlaneService>;

/// Renders an extractor rejection as the gear's own validation problem, so a
/// malformed or mis-typed body is answered `400` in `application/problem+json`
/// with the gateway error source rather than axum's plain-text default.
fn rejected_json(rejection: &JsonRejection) -> Response {
    error::problem_response(
        &crate::domain::error::DomainError::Validation(rejection.body_text()),
        None,
    )
}

/// The calling tenant, as a string.
#[must_use]
pub fn tenant_id(ctx: &SecurityContext) -> String {
    ctx.subject_tenant_id().to_string()
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`.
///
/// # Errors
/// Returns a problem response when validation fails.
pub async fn create_upstream(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    body: Result<Json<CreateUpstream>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejected_json(&rejection),
    };
    let tenant = tenant_id(&ctx);
    let provided_alias = body.alias.clone();
    match service
        .create_upstream(&tenant, body.into_domain(&tenant), provided_alias)
        .await
    {
        Ok(upstream) => dto::json_response(StatusCode::CREATED, &UpstreamResponse::from(upstream)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/upstreams`.
pub async fn list_upstreams(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
) -> Response {
    match service.list_upstreams(&tenant_id(&ctx)).await {
        Ok(items) => dto::json_response(
            StatusCode::OK,
            &ListResponse::new(items.clone(), items.len()),
        ),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/upstreams/{id}`.
pub async fn get_upstream(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_upstream(&tenant_id(&ctx), id).await {
        Ok(upstream) => dto::json_response(StatusCode::OK, &UpstreamResponse::from(upstream)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `PUT /oagw/v1/upstreams/{id}`.
///
/// # Errors
/// Returns a problem response when validation fails.
pub async fn replace_upstream(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    body: Result<Json<CreateUpstream>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejected_json(&rejection),
    };
    let tenant = tenant_id(&ctx);
    let provided_alias = body.alias.clone();
    match service
        .replace_upstream(&tenant, id, body.into_domain(&tenant), provided_alias)
        .await
    {
        Ok(upstream) => dto::json_response(StatusCode::OK, &UpstreamResponse::from(upstream)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `DELETE /oagw/v1/upstreams/{id}`.
pub async fn delete_upstream(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.delete_upstream(&tenant_id(&ctx), id).await {
        Ok(()) => dto::empty_response(StatusCode::NO_CONTENT),
        Err(e) => error::problem_response(&e, None),
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`.
///
/// # Errors
/// Returns a problem response when validation fails.
pub async fn create_route(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    body: Result<Json<CreateRoute>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejected_json(&rejection),
    };
    let tenant = tenant_id(&ctx);
    match body.into_domain(&tenant) {
        Ok(route) => match service.create_route(&tenant, route).await {
            Ok(route) => dto::json_response(StatusCode::CREATED, &RouteResponse::from(route)),
            Err(e) => error::problem_response(&e, None),
        },
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/routes`.
pub async fn list_routes(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
) -> Response {
    match service.list_routes(&tenant_id(&ctx)).await {
        Ok(items) => dto::json_response(
            StatusCode::OK,
            &ListResponse::new(items.clone(), items.len()),
        ),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/routes/{id}`.
pub async fn get_route(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_route(&tenant_id(&ctx), id).await {
        Ok(route) => dto::json_response(StatusCode::OK, &RouteResponse::from(route)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `PUT /oagw/v1/routes/{id}`.
///
/// # Errors
/// Returns a problem response when validation fails.
pub async fn replace_route(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    body: Result<Json<CreateRoute>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejected_json(&rejection),
    };
    let tenant = tenant_id(&ctx);
    let route = match body.into_domain(&tenant) {
        Ok(route) => route,
        Err(e) => return error::problem_response(&e, None),
    };
    match service.replace_route(&tenant, id, route).await {
        Ok(route) => dto::json_response(StatusCode::OK, &RouteResponse::from(route)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `DELETE /oagw/v1/routes/{id}`.
pub async fn delete_route(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.delete_route(&tenant_id(&ctx), id).await {
        Ok(()) => dto::empty_response(StatusCode::NO_CONTENT),
        Err(e) => error::problem_response(&e, None),
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`.
///
/// # Errors
/// Returns a problem response when validation fails.
pub async fn create_plugin(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    body: Result<Json<CreatePlugin>, JsonRejection>,
) -> Response {
    let body = match body {
        Ok(Json(body)) => body,
        Err(rejection) => return rejected_json(&rejection),
    };
    let tenant = tenant_id(&ctx);
    match service
        .create_plugin(&tenant, body.into_domain(&tenant))
        .await
    {
        Ok(plugin) => dto::json_response(StatusCode::CREATED, &PluginResponse::from(plugin)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/plugins`.
pub async fn list_plugins(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
) -> Response {
    match service.list_plugins(&tenant_id(&ctx)).await {
        Ok(items) => dto::json_response(
            StatusCode::OK,
            &ListResponse::new(items.clone(), items.len()),
        ),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/plugins/{id}`.
pub async fn get_plugin(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_plugin(&tenant_id(&ctx), id).await {
        Ok(plugin) => dto::json_response(StatusCode::OK, &PluginResponse::from(plugin)),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `GET /oagw/v1/plugins/{id}/source`.
pub async fn get_plugin_source(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_plugin(&tenant_id(&ctx), id).await {
        Ok(plugin) => dto::json_response(
            StatusCode::OK,
            &serde_json::json!({
                "id": plugin.id,
                "name": plugin.name,
                "type": plugin.plugin_type,
                "source_code": plugin.source_code,
            }),
        ),
        Err(e) => error::problem_response(&e, None),
    }
}

/// `DELETE /oagw/v1/plugins/{id}`.
pub async fn delete_plugin(
    Extension(service): Extension<Service>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.delete_plugin(&tenant_id(&ctx), id).await {
        Ok(()) => dto::empty_response(StatusCode::NO_CONTENT),
        Err(e) => error::problem_response(&e, None),
    }
}
