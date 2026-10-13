//! Reaction handlers.

use super::prelude::*;

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction` — set a reaction.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    Ok(Json(svc.set_reaction(&ctx, id, msg_id, &body.reaction).await.map_err(err)?.into()))
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` — remove a reaction.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_reaction(&ctx, id, msg_id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}
