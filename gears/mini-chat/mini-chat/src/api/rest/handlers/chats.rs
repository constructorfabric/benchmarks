//! Chat CRUD handlers (OWNER: REST CRUD work package).

use std::sync::Arc;

use axum::Extension;
use axum::response::{IntoResponse, Response};
use http::{StatusCode, header};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::odata::OData;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{CreateChatReq, UpdateChatReq};
use crate::domain::service::Services;

/// `GET /v1/chats`.
pub async fn list_chats(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    OData(query): OData,
) -> ApiResult<Response> {
    let page = svc.chats.list(&ctx, query).await?;
    Ok(axum::Json(page).into_response())
}

/// `POST /v1/chats` → 201 + `Location`.
pub async fn create_chat(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(req): Json<CreateChatReq>,
) -> ApiResult<Response> {
    let dto = svc.chats.create(&ctx, req).await?;
    let location = format!(
        "{}/v1/chats/{}",
        svc.deps.cfg.url_prefix.trim_end_matches('/'),
        dto.id
    );
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        axum::Json(dto),
    )
        .into_response())
}

/// `GET /v1/chats/{id}`.
pub async fn get_chat(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let dto = svc.chats.get(&ctx, id).await?;
    Ok(axum::Json(dto).into_response())
}

/// `PATCH /v1/chats/{id}` (title only; unknown body fields are ignored).
pub async fn update_chat(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatReq>,
) -> ApiResult<Response> {
    let dto = svc.chats.update_title(&ctx, id, &req.title).await?;
    Ok(axum::Json(dto).into_response())
}

/// `DELETE /v1/chats/{id}` → 204.
pub async fn delete_chat(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    svc.chats.delete(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
#[path = "chats_tests.rs"]
mod chats_tests;
