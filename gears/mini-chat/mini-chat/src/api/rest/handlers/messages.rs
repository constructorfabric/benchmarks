//! Message history, reaction and turn status handlers (DESIGN section 3.3
//! "List Messages", "Message Reaction API", "Turn Status API").

use axum::extract::Extension;
use toolkit::api::canonical_prelude::{ApiResult, JsonBody, JsonPage, OData, StatusCode};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::RestState;
use crate::api::rest::dto::{MessageDto, ReactionDto, SetReactionReq, TurnStatusResponse};

/// `GET {prefix}/v1/chats/{id}/messages` — one page of the chat's messages.
/// `$select` is validated by the extractor and ignored.
///
/// # Errors
///
/// 400 (path, `OData` query), 403, 404 (chat), 500, 503.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(chat_id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<JsonPage<MessageDto>> {
    let page = state.messages.list_messages(&ctx, chat_id, &query).await?;
    Ok(JsonBody::from(page.map_items(MessageDto::from)))
}

/// `PUT {prefix}/v1/chats/{id}/messages/{msg_id}/reaction` — upsert.
///
/// # Errors
///
/// 400 (`INVALID_REACTION`, `reaction_target`, path, extractor), 403, 404
/// (chat, message), 415 / 422 (extractor), 500, 503.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, message_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetReactionReq>,
) -> ApiResult<JsonBody<ReactionDto>> {
    let reaction = state
        .messages
        .set_reaction(&ctx, chat_id, message_id, &req.reaction)
        .await?;
    Ok(JsonBody::from(ReactionDto::from(reaction)))
}

/// `DELETE {prefix}/v1/chats/{id}/messages/{msg_id}/reaction` — idempotent,
/// 204.
///
/// # Errors
///
/// 400 (`reaction_target`, path), 403, 404 (chat, message), 500, 503.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, message_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state
        .messages
        .delete_reaction(&ctx, chat_id, message_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET {prefix}/v1/chats/{id}/turns/{request_id}`.
///
/// # Errors
///
/// 400 (path), 403, 404 (chat, turn), 500, 503.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<JsonBody<TurnStatusResponse>> {
    let status = state
        .messages
        .get_turn_status(&ctx, chat_id, request_id)
        .await?;
    Ok(JsonBody::from(TurnStatusResponse::from(status)))
}
