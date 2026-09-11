//! Management-API handlers: upstream / route / plugin CRUD.
//!
//! Each handler forwards [`SecurityContext`] plus the parsed draft to
//! [`crate::domain::service::Service`] and maps the outcome onto a status
//! code: `201` on create, `200` on read and replace, `204` on delete, and the
//! problem body of whatever [`DomainError`] the service produced.

use axum::Extension;
use axum::extract::Path;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{ApiState, parse_json, problem};
use crate::domain::list::ListQuery;
use crate::domain::model::{PluginDefinition, Route, Upstream};
use crate::domain::service;
use crate::error::DomainError;

/// `POST /oagw/v1/upstreams`
pub async fn create_upstream(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    body: axum::body::Bytes,
) -> Response {
    let draft: Upstream = match parse_json(&body) {
        Ok(draft) => draft,
        Err(err) => return problem(err, &uri, None),
    };
    match state.service.create_upstream(&security, draft).await {
        Ok(created) => created_at("/oagw/v1/upstreams", &created.id),
        Err(err) => problem(err, &uri, None),
    }
}

/// `GET /oagw/v1/upstreams`
pub async fn list_upstreams(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
) -> Response {
    let query = ListQuery::parse(uri.query());
    ok(state
        .service
        .list_upstreams(&security.subject_tenant_id(), &query))
}

/// `GET /oagw/v1/upstreams/{id}`
pub async fn get_upstream(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .get_upstream(&security.subject_tenant_id(), &id)
            .map(ok)
    })
}

/// `PUT /oagw/v1/upstreams/{id}`
pub async fn replace_upstream(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let replacement: Upstream = match parse_json(&body) {
        Ok(replacement) => replacement,
        Err(err) => return problem(err, &uri, None),
    };
    let Some(id) = service::parse_instance_id(&id).or_else(|| Uuid::parse_str(&id).ok()) else {
        return problem(not_a_resource_id(), &uri, None);
    };
    match state
        .service
        .replace_upstream(&security, &id, replacement)
        .await
    {
        Ok(updated) => ok(updated),
        Err(err) => problem(err, &uri, None),
    }
}

/// `DELETE /oagw/v1/upstreams/{id}`
pub async fn delete_upstream(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .delete_upstream(&security.subject_tenant_id(), &id)
            .map(|_| no_content())
    })
}

/// `POST /oagw/v1/routes`
pub async fn create_route(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    body: axum::body::Bytes,
) -> Response {
    let draft: Route = match parse_json(&body) {
        Ok(draft) => draft,
        Err(err) => return problem(err, &uri, None),
    };
    match state.service.create_route(&security, draft) {
        Ok(created) => created_at("/oagw/v1/routes", &created.id),
        Err(err) => problem(err, &uri, None),
    }
}

/// `GET /oagw/v1/routes`
pub async fn list_routes(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
) -> Response {
    let query = ListQuery::parse(uri.query());
    ok(state
        .service
        .list_routes(&security.subject_tenant_id(), &query))
}

/// `GET /oagw/v1/routes/{id}`
pub async fn get_route(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .get_route(&security.subject_tenant_id(), &id)
            .map(ok)
    })
}

/// `PUT /oagw/v1/routes/{id}`
pub async fn replace_route(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let replacement: Route = match parse_json(&body) {
        Ok(replacement) => replacement,
        Err(err) => return problem(err, &uri, None),
    };
    with_id(&uri, &id, |id| {
        state
            .service
            .replace_route(&security.subject_tenant_id(), &id, replacement)
            .map(ok)
    })
}

/// `DELETE /oagw/v1/routes/{id}`
pub async fn delete_route(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .delete_route(&security.subject_tenant_id(), &id)
            .map(|_| no_content())
    })
}

/// `POST /oagw/v1/plugins`
pub async fn create_plugin(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    body: axum::body::Bytes,
) -> Response {
    let draft: PluginDefinition = match parse_json(&body) {
        Ok(draft) => draft,
        Err(err) => return problem(err, &uri, None),
    };
    match state
        .service
        .create_plugin(&security.subject_tenant_id(), draft)
    {
        Ok(created) => created_at("/oagw/v1/plugins", &created.id),
        Err(err) => problem(err, &uri, None),
    }
}

/// `GET /oagw/v1/plugins`
pub async fn list_plugins(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
) -> Response {
    let query = ListQuery::parse(uri.query());
    ok(state
        .service
        .list_plugins(&security.subject_tenant_id(), &query))
}

/// `GET /oagw/v1/plugins/{id}`
pub async fn get_plugin(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .get_plugin(&security.subject_tenant_id(), &id)
            .map(ok)
    })
}

/// `DELETE /oagw/v1/plugins/{id}`
pub async fn delete_plugin(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .delete_plugin(&security.subject_tenant_id(), &id)
            .map(|_| no_content())
    })
}

/// `GET /oagw/v1/plugins/{id}/source`
pub async fn get_plugin_source(
    uri: Uri,
    Extension(state): Extension<ApiState>,
    Extension(security): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    with_id(&uri, &id, |id| {
        state
            .service
            .plugin_source(&security.subject_tenant_id(), &id)
            .map(ok)
    })
}

/// Run `f` over the parsed id, or answer `400` when the path parameter is not
/// one.
fn with_id<F>(uri: &Uri, raw: &str, f: F) -> Response
where
    F: FnOnce(Uuid) -> Result<Response, DomainError>,
{
    match service::parse_instance_id(raw).or_else(|| Uuid::parse_str(raw).ok()) {
        Some(id) => match f(id) {
            Ok(response) => response,
            Err(err) => problem(err, uri, None),
        },
        None => problem(not_a_resource_id(), uri, None),
    }
}

/// The `400` a non-id path parameter produces.
fn not_a_resource_id() -> DomainError {
    DomainError::new(
        crate::error::ErrorKind::Validation,
        "the path parameter is not a resource id",
    )
}

/// `201 Created` with the created resource's id and a `Location` header.
fn created_at(collection: &str, id: &Uuid) -> Response {
    let mut response = (StatusCode::CREATED, axum::Json(id)).into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&format!("{collection}/{id}")) {
        response
            .headers_mut()
            .insert(axum::http::header::LOCATION, value);
    }
    response
}

fn ok<T: serde::Serialize>(value: T) -> Response {
    (StatusCode::OK, axum::Json(value)).into_response()
}

fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_created_response_carries_the_location_of_the_new_resource() {
        let id = Uuid::new_v4();
        let response = created_at("/oagw/v1/upstreams", &id);
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|value| value.to_str().ok()),
            Some(format!("/oagw/v1/upstreams/{id}").as_str())
        );
    }

    #[test]
    fn a_delete_is_204() {
        assert_eq!(no_content().status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn an_id_is_either_a_uuid_or_a_gts_instance_id() {
        let id = Uuid::new_v4();
        assert_eq!(service::parse_instance_id(&id.to_string()), Some(id));
        let gts = format!("{}{id}", crate::ids::UPSTREAM_RESOURCE_TYPE);
        assert_eq!(service::parse_instance_id(&gts), Some(id));
        assert_eq!(service::parse_instance_id("not-an-id"), None);
    }
}
