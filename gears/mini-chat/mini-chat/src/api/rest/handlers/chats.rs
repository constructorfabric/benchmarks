//! Chat CRUD handlers. Errors are rendered as canonical problems
//! (`From<DomainError>` / `From<ListError>` for `CanonicalError`).

use axum::Extension;
use axum::http::Uri;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ChatDetailDto, CreateChatReq, UpdateChatReq};
use crate::domain::services::Services;

/// `POST /v1/chats` → 201 + `Location`.
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Json(req): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let view = svc.chats.create(&ctx, req.title, req.model).await?;
    let id = view.chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(view), &uri, &id))
}

/// `GET /v1/chats`.
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    OData(query): OData,
) -> ApiResult<JsonPage<ChatDetailDto>> {
    let page = svc.chats.list(&ctx, &query).await?;
    Ok(Json(toolkit_odata::Page {
        items: page.items.into_iter().map(ChatDetailDto::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET /v1/chats/{id}`.
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    let view = svc.chats.get(&ctx, id).await?;
    Ok(Json(view.into()))
}

/// `PATCH /v1/chats/{id}` (title only).
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    let view = svc.chats.update_title(&ctx, id, req.title).await?;
    Ok(Json(view.into()))
}

/// `DELETE /v1/chats/{id}` → 204.
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    svc.chats.delete(&ctx, id).await?;
    Ok(no_content())
}
