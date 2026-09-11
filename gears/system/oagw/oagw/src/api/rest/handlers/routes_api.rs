//! Management handlers for routes.

use crate::api::extract::{CallerContext, QueryOptions, parse_json};
use crate::api::rest::dto::{CollectionEnvelope, RouteView};
use crate::api::rest::error::gateway_error_response;
use crate::api::rest::handlers::upstreams::{MAX_BODY, Shared, json_response};
use crate::domain::error::OagwError;
use crate::domain::model::Route;
use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// `POST /upstreams/{upstream_id}/routes`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn create(
    Extension(service): Extension<Shared>,
    Path(upstream_id): Path<String>,
    uri: Uri,
    caller: CallerContext,
    body: Bytes,
) -> Response {
    create_under(service, upstream_id, uri, caller, body).await
}

/// `POST /routes`, with the upstream named in the body.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn create_standalone(
    Extension(service): Extension<Shared>,
    uri: Uri,
    caller: CallerContext,
    body: Bytes,
) -> Response {
    let route: Route = match parse_json(&body, MAX_BODY) {
        Ok(value) => value,
        Err(error) => return failure(&error, uri.path()),
    };
    if route.upstream_id.trim().is_empty() {
        return failure(
            &OagwError::ValidationError("upstream_id must name an upstream".to_owned()),
            uri.path(),
        );
    }
    create_under(service, route.upstream_id.clone(), uri, caller, body).await
}

/// Creates a route under an upstream named by the path.
async fn create_under(
    service: Shared,
    upstream_id: String,
    uri: Uri,
    caller: CallerContext,
    body: Bytes,
) -> Response {
    let mut route: Route = match parse_json(&body, MAX_BODY) {
        Ok(value) => value,
        Err(error) => return failure(&error, uri.path()),
    };
    route.tenant_id = caller.tenant_id;
    match service
        .create_route(caller.tenant_id, &upstream_id, route)
        .await
    {
        Ok(created) => json_response(StatusCode::CREATED, &RouteView::from(created)),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `GET /routes`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn list(
    Extension(service): Extension<Shared>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.list_routes(caller.tenant_id).await {
        Ok(all) => {
            let query = QueryOptions::parse(uri.query().unwrap_or_default());
            let mut views: Vec<RouteView> = all.into_iter().map(RouteView::from).collect();
            views.sort_by(|left, right| {
                right
                    .priority
                    .cmp(&left.priority)
                    .then_with(|| left.id.cmp(&right.id))
            });
            let total = views.len();
            let offset = query.offset();
            let page: Vec<RouteView> = views
                .into_iter()
                .skip(offset)
                .take(query.page_size())
                .collect();
            json_response(
                StatusCode::OK,
                &CollectionEnvelope::paginate(page, Some(total), offset),
            )
        }
        Err(error) => failure(&error, uri.path()),
    }
}

/// `GET /routes/{id}`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn get(
    Extension(service): Extension<Shared>,
    Path(id): Path<String>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.get_route(caller.tenant_id, &id).await {
        Ok(found) => json_response(StatusCode::OK, &RouteView::from(found)),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `PUT /routes/{id}`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn replace(
    Extension(service): Extension<Shared>,
    Path(id): Path<String>,
    uri: Uri,
    caller: CallerContext,
    body: Bytes,
) -> Response {
    let mut route: Route = match parse_json(&body, MAX_BODY) {
        Ok(value) => value,
        Err(error) => return failure(&error, uri.path()),
    };
    route.tenant_id = caller.tenant_id;
    match service.replace_route(caller.tenant_id, &id, route).await {
        Ok(updated) => json_response(StatusCode::OK, &RouteView::from(updated)),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `DELETE /routes/{id}`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn delete(
    Extension(service): Extension<Shared>,
    Path(id): Path<String>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.delete_route(caller.tenant_id, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error, uri.path()),
    }
}

fn failure(error: &OagwError, instance: &str) -> Response {
    gateway_error_response(error, Some(instance))
}
