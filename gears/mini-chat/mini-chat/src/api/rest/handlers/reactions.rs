//! Message reaction handlers.

use axum::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{MiniChatReactionDto, SetReactionReq};
use crate::domain::services::Services;

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    let row = svc.reactions.set(&ctx, id, msg_id, &req.reaction).await?;
    Ok(Json(MiniChatReactionDto::try_from(row)?))
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` → 204.
///
/// # Errors
/// Canonical problem for validation, authorization and storage failures.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.reactions.remove(&ctx, id, msg_id).await?;
    Ok(no_content())
}
