//! Handlers of the chat, message and reaction endpoints. The caller comes
//! from the `SecurityContext` the api-gateway auth layer injects. Extraction
//! order (earlier wins): path, JSON body / `OData` query; the services then
//! validate fields, ask the PDP and read or write.

use std::sync::Arc;

use axum::Json;
use axum::extract::Extension;
use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::*;
use toolkit::api::odata::OData;
use toolkit::api::rest::extract::{Json as ReqJson, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    ChatDetailDto, CreateChatReq, MiniChatMessageDto, MiniChatReactionDto, SetReactionReq,
    UpdateChatReq,
};
use crate::domain::chats::ChatsService;
use crate::domain::messages::MessagesService;
use crate::domain::reactions::ReactionsService;

/// `POST {prefix}/v1/chats`
///
/// # Errors
/// Canonical `Problem`: 400 (`INVALID_TITLE`, `INVALID_MODEL`), 403, 415, 422,
/// 503 when the PDP is unreachable, 500.
pub async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatsService>>,
    uri: Uri,
    ReqJson(body): ReqJson<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = chats
        .create_chat(&ctx, body.title.as_deref(), body.model.as_deref())
        .await?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

/// `GET {prefix}/v1/chats`
///
/// # Errors
/// Canonical `Problem`: 400 for a bad `OData` query, 403, 503, 500.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatsService>>,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = chats.list_chats(&ctx, &query).await?;
    Ok(Json(Page {
        items: page.items.into_iter().map(ChatDetailDto::from).collect(),
        page_info: page.page_info,
    }))
}

/// `GET {prefix}/v1/chats/{id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`invalid_path_params`), 403, 404, 503, 500.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatsService>>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(chats.get_chat(&ctx, id).await?.into()))
}

/// `PATCH {prefix}/v1/chats/{id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`INVALID_TITLE`, `invalid_path_params`), 403, 404,
/// 415, 422, 503, 500.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatsService>>,
    Path(id): Path<Uuid>,
    ReqJson(body): ReqJson<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(
        chats.update_title(&ctx, id, &body.title).await?.into(),
    ))
}

/// `DELETE {prefix}/v1/chats/{id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`invalid_path_params`, oversized cleanup payload),
/// 403, 404, 503, 500.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatsService>>,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    chats.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET {prefix}/v1/chats/{id}/messages`
///
/// # Errors
/// Canonical `Problem`: 400 (bad `OData` query, `invalid_path_params`), 403, 404,
/// 503, 500.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(messages): Extension<Arc<MessagesService>>,
    Path(id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = messages.list_messages(&ctx, id, &query).await?;
    Ok(Json(Page {
        items: page
            .items
            .into_iter()
            .map(MiniChatMessageDto::from)
            .collect(),
        page_info: page.page_info,
    }))
}

/// `PUT {prefix}/v1/chats/{id}/messages/{msg_id}/reaction`
///
/// # Errors
/// Canonical `Problem`: 400 (`INVALID_REACTION`, reaction target not an
/// assistant message, `invalid_path_params`), 403, 404, 415, 422, 503, 500.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(reactions): Extension<Arc<ReactionsService>>,
    Path((id, message_id)): Path<(Uuid, Uuid)>,
    ReqJson(body): ReqJson<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    let reaction = reactions
        .set_reaction(&ctx, id, message_id, &body.reaction)
        .await?;
    Ok(Json(reaction.into()))
}

/// `DELETE {prefix}/v1/chats/{id}/messages/{msg_id}/reaction`
///
/// # Errors
/// Canonical `Problem`: 400 (reaction target not an assistant message,
/// `invalid_path_params`), 403, 404, 503, 500.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(reactions): Extension<Arc<ReactionsService>>,
    Path((id, message_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    reactions.delete_reaction(&ctx, id, message_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
