//! Management handlers for `/oagw/v1/plugins`.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::{CreatePluginDto, PluginDto};
use crate::api::rest::handlers::upstreams::ListQuery;
use crate::domain::model::Plugin;
use crate::domain::service::ControlPlaneService;

/// `POST /oagw/v1/plugins`
///
/// # Errors
///
/// Returns 400 when the name or kind is empty.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Json(body): Json<CreatePluginDto>,
) -> Result<(StatusCode, Json<PluginDto>), crate::domain::error::DomainError> {
    let created = svc
        .create_plugin(
            ctx.subject_tenant_id(),
            Plugin {
                id: uuid::Uuid::nil(),
                tenant_id: ctx.subject_tenant_id(),
                name: body.name,
                plugin_type: body.plugin_type,
                source: body.source,
                gc_eligible_at: None,
                created_at: 0,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(PluginDto::from(created))))
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
///
/// Propagates store failures.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<PluginDto>>, crate::domain::error::DomainError> {
    let rows = svc
        .list_plugins(ctx.subject_tenant_id(), &query.into_filter())
        .await?;
    Ok(Json(rows.into_iter().map(PluginDto::from).collect()))
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns 404 when absent or owned by another tenant.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<PluginDto>, crate::domain::error::DomainError> {
    Ok(Json(PluginDto::from(
        svc.get_plugin(ctx.subject_tenant_id(), id).await?,
    )))
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
///
/// Returns 404 when absent or owned by another tenant.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<axum::response::Response, crate::domain::error::DomainError> {
    let source = svc.plugin_source(ctx.subject_tenant_id(), id).await?;
    let body = axum::body::Body::from(source);
    Ok((
        [(http::header::CONTENT_TYPE, "text/x-python; charset=utf-8")],
        body,
    )
        .into_response())
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns 404 when absent and 409 while an upstream or route still binds it.
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<ControlPlaneService>,
    Path(id): Path<uuid::Uuid>,
) -> Result<impl IntoResponse, crate::domain::error::DomainError> {
    svc.delete_plugin(ctx.subject_tenant_id(), id).await?;
    Ok(StatusCode::NO_CONTENT)
}
