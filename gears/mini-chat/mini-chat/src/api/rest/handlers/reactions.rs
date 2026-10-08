//! `PUT` / `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`.

use std::sync::Arc;

use axum::Extension;
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{ApiResult, no_content};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{MiniChatReactionDto, SetReactionReq};
use crate::domain::services::AppServices;

/// Set (or replace) the caller's reaction on an assistant message.
///
/// # Errors
/// 400 (`INVALID_REACTION`, `reaction_target`, non-UUID path), 404
/// (`chat` / `message`), 422 (bad body), 403 / 503 from authorization, 500.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    let view = svc.reactions.set(&ctx, id, msg_id, &req.reaction).await?;
    Ok(Json(view.try_into()?))
}

/// Remove the caller's reaction (204, idempotent).
///
/// # Errors
/// 400 (`reaction_target`, non-UUID path), 404 (`chat` / `message`), 403 /
/// 503 from authorization, 500.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.reactions.remove(&ctx, id, msg_id).await?;
    Ok(no_content())
}
