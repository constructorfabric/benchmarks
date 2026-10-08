//! Chat CRUD handlers (DESIGN section 3.3).

use std::sync::Arc;

use axum::Extension;
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{ApiResult, OData, created_json};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ChatDetailDto, CreateChatReq, UpdateChatReq};
use crate::domain::services::chat_service::CreateChat;
use crate::gear::AppState;

/// `GET {prefix}/v1/chats`
///
/// # Errors
/// Canonical 400 (`OData` query), 403, 500.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = st.chats.list(&ctx, query).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// `POST {prefix}/v1/chats` — 201 with `Location: {request path}/{id}`.
///
/// # Errors
/// Canonical 400 (title, model), 403, 415/422 (body), 500.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Json(body): Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = st
        .chats
        .create(
            &ctx,
            CreateChat {
                title: body.title,
                model: body.model,
            },
        )
        .await?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

/// `GET {prefix}/v1/chats/{id}`
///
/// # Errors
/// Canonical 400 (path), 403, 404, 500.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(st.chats.get(&ctx, id).await?.into()))
}

/// `PATCH {prefix}/v1/chats/{id}` — only `title` is applied.
///
/// # Errors
/// Canonical 400 (path, title), 403, 404, 415/422 (body), 500.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(
        st.chats.update_title(&ctx, id, &body.title).await?.into(),
    ))
}

/// `DELETE {prefix}/v1/chats/{id}` — 204.
///
/// # Errors
/// Canonical 400 (path, cleanup payload too large), 403, 404, 500.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    st.chats.delete(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
