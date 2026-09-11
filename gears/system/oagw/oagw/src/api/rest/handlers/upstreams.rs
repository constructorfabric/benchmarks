//! Management handlers for upstreams.

use crate::api::extract::{CallerContext, QueryOptions, parse_json};
use crate::api::rest::dto::{CollectionEnvelope, CreatedResource, UpstreamView};
use crate::api::rest::error::gateway_error_response;
use crate::domain::error::OagwError;
use crate::domain::model::Upstream;
use crate::domain::services::ControlPlaneService;
use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::http::header;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::sync::Arc;

/// The state every handler sees.
pub type Shared = Arc<ControlPlaneService>;

/// Serialises a value into a JSON response.
pub fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let body = serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"error\":\"serialization failed\"}".to_owned());
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|error| {
            (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response()
        })
}

/// Wraps a domain failure into its problem document.
fn failure(error: &OagwError, instance: &str) -> Response {
    gateway_error_response(error, Some(instance))
}

/// `POST /upstreams`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn create(
    Extension(service): Extension<Shared>,
    uri: Uri,
    caller: CallerContext,
    body: Bytes,
) -> Response {
    let mut upstream: Upstream = match parse_json(&body, MAX_BODY) {
        Ok(value) => value,
        Err(error) => return failure(&error, uri.path()),
    };
    upstream.tenant_id = caller.tenant_id;
    match service.create_upstream(caller.tenant_id, upstream).await {
        Ok(created) => {
            let view = UpstreamView::from(created.clone());
            let mut response = json_response(StatusCode::CREATED, &CreatedResource::new(view));
            if let Ok(value) =
                axum::http::HeaderValue::from_str(&format!("/oagw/v1/upstreams/{}", created.id))
            {
                response.headers_mut().insert(header::LOCATION, value);
            }
            response
        }
        Err(error) => failure(&error, uri.path()),
    }
}

/// `GET /upstreams`.
///
/// # Errors
///
/// Returns the problem document for any domain failure.
pub async fn list(
    Extension(service): Extension<Shared>,
    uri: Uri,
    caller: CallerContext,
) -> Response {
    match service.list_upstreams(caller.tenant_id).await {
        Ok(all) => {
            let mut all: Vec<UpstreamView> = all.into_iter().map(UpstreamView::from).collect();
            apply_ordering(&mut all, "");
            let query = QueryOptions::parse(uri.query().unwrap_or_default());
            let total = all.len();
            let offset = query.offset();
            let page: Vec<UpstreamView> = all
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

/// `GET /upstreams/{id}`.
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
    match service.get_upstream(caller.tenant_id, &id).await {
        Ok(found) => json_response(StatusCode::OK, &UpstreamView::from(found)),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `PUT /upstreams/{id}`.
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
    let mut upstream: Upstream = match parse_json(&body, MAX_BODY) {
        Ok(value) => value,
        Err(error) => return failure(&error, uri.path()),
    };
    upstream.tenant_id = caller.tenant_id;
    match service
        .replace_upstream(caller.tenant_id, &id, upstream)
        .await
    {
        Ok(updated) => json_response(StatusCode::OK, &UpstreamView::from(updated)),
        Err(error) => failure(&error, uri.path()),
    }
}

/// `DELETE /upstreams/{id}`.
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
    match service.delete_upstream(caller.tenant_id, &id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failure(&error, uri.path()),
    }
}

/// Ordering hook shared by the collection handlers; upstreams order by alias.
fn apply_ordering(items: &mut [UpstreamView], _clause: &str) {
    items.sort_by(|left, right| left.alias.cmp(&right.alias));
}

/// Body limit for management writes; upstream documents are small.
pub const MAX_BODY: usize = 1_000_000;
