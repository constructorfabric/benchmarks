//! Chat CRUD handlers (DESIGN section 3.3 "Create Chat", "List Chats", "Get
//! Chat", "Update Chat Title", "Delete Chat").

use axum::extract::Extension;
use axum::http::Uri;
use toolkit::api::canonical_prelude::{
    ApiResult, IntoResponse, JsonBody, JsonPage, OData, StatusCode, created_json,
};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::RestState;
use crate::api::rest::dto::{ChatDetailDto, CreateChatReq, UpdateChatReq};

/// `POST {prefix}/v1/chats` — 201 with the `ChatDetail` and a `Location`
/// header: the request path as the gear sees it plus the new id.
///
/// # Errors
///
/// 400 (`INVALID_TITLE`, `INVALID_MODEL`, extractor), 403, 415 / 422
/// (extractor), 500, 503.
pub async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    uri: Uri,
    Json(req): Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = state.chats.create(&ctx, req.title, req.model).await?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

/// `GET {prefix}/v1/chats` — one page of the caller's chats. `$select` is
/// validated by the extractor and ignored.
///
/// # Errors
///
/// 400 (`OData` query), 403, 500, 503.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    OData(query): OData,
) -> ApiResult<JsonPage<ChatDetailDto>> {
    let page = state.chats.list(&ctx, &query).await?;
    Ok(JsonBody::from(page.map_items(ChatDetailDto::from)))
}

/// `GET {prefix}/v1/chats/{id}`.
///
/// # Errors
///
/// 400 (path), 403, 404 (chat), 500, 503.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(id): Path<Uuid>,
) -> ApiResult<JsonBody<ChatDetailDto>> {
    let chat = state.chats.get(&ctx, id).await?;
    Ok(JsonBody::from(ChatDetailDto::from(chat)))
}

/// `PATCH {prefix}/v1/chats/{id}` — renames the chat; other body fields are
/// ignored.
///
/// # Errors
///
/// 400 (`INVALID_TITLE`, path, extractor), 403, 404 (chat), 415 / 422
/// (extractor), 500, 503.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateChatReq>,
) -> ApiResult<JsonBody<ChatDetailDto>> {
    let chat = state.chats.update_title(&ctx, id, &req.title).await?;
    Ok(JsonBody::from(ChatDetailDto::from(chat)))
}

/// `DELETE {prefix}/v1/chats/{id}` — soft delete, 204.
///
/// # Errors
///
/// 400 (path), 403, 404 (chat), 500, 503.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    state.chats.delete(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
