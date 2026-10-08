//! Chat CRUD handlers (`/v1/chats`, `/v1/chats/{id}`).

use std::sync::Arc;

use axum::Extension;
use axum::http::Uri;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{ApiResult, OData, created_json, no_content};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ChatDetailDto, CreateChatReq, UpdateChatReq};
use crate::domain::services::AppServices;

/// `POST /v1/chats` — 201 + `Location: {request path}/{id}`.
///
/// # Errors
/// 400 (`INVALID_TITLE`, `INVALID_MODEL`, malformed JSON), 415, 422,
/// 403 / 503 from authorization, 500.
pub async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    uri: Uri,
    Json(req): Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc.chats.create(&ctx, req.title, req.model).await?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

/// `GET /v1/chats` — the caller's chats (`OData` `$filter` / `$orderby` over
/// `updated_at`, `id`, `title`; `$select` is accepted and ignored).
///
/// # Errors
/// 400 (`OData` query), 403 / 503 from authorization, 500.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = svc.chats.list(&ctx, &query).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// `GET /v1/chats/{id}` — chat metadata + `message_count`.
///
/// # Errors
/// 404 (`chat`), 400 (non-UUID id), 403 / 503 from authorization, 500.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    let chat = svc.chats.get(&ctx, id).await?;
    Ok(Json(chat.into()))
}

/// `PATCH /v1/chats/{id}` — rename (title only; other fields ignored).
///
/// # Errors
/// 404 (`chat`), 400 (`INVALID_TITLE`, non-UUID id, malformed JSON), 415,
/// 422, 403 / 503 from authorization, 500.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    let chat = svc.chats.rename(&ctx, id, &req.title).await?;
    Ok(Json(chat.into()))
}

/// `DELETE /v1/chats/{id}` — soft delete + asynchronous provider cleanup.
///
/// # Errors
/// 404 (`chat`, also when already deleted), 400 (non-UUID id), 403 / 503
/// from authorization, 500.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    svc.chats.delete(&ctx, id).await?;
    Ok(no_content())
}
