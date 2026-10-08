//! Chat CRUD handlers.

use super::prelude::*;

/// `POST /v1/chats` — create a chat.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = svc.create_chat(&ctx, body.title, body.model).await.map_err(err)?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), view.chat.id);
    let dto = ChatDetailDto::from(view);
    Ok((StatusCode::CREATED, [(header::LOCATION, location)], Json(dto)).into_response())
}

/// `GET /v1/chats` — list the caller's chats.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = svc.list_chats(&ctx, query).await.map_err(err)?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// `GET /v1/chats/{id}` — fetch one chat.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.get_chat(&ctx, id).await.map_err(err)?.into()))
}

/// `PATCH /v1/chats/{id}` — rename a chat.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.update_chat_title(&ctx, id, &body.title).await.map_err(err)?.into()))
}

/// `DELETE /v1/chats/{id}` — soft-delete a chat.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<StatusCode> {
    svc.delete_chat(&ctx, id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}
