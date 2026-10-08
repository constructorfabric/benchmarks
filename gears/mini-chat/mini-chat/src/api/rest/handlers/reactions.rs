//! Message reaction handlers (DESIGN section 3.3, Message Reaction API).

use std::sync::Arc;

use axum::Extension;
use axum::http::StatusCode;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ReactionDto, SetReactionReq};
use crate::gear::AppState;

/// `PUT {prefix}/v1/chats/{id}/messages/{msg_id}/reaction`
///
/// # Errors
/// Canonical 400 (path, reaction value, non-assistant target), 403, 404,
/// 415/422 (body), 500.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, msg_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<SetReactionReq>,
) -> ApiResult<Json<ReactionDto>> {
    let reaction = st
        .reactions
        .set(&ctx, chat_id, msg_id, &body.reaction)
        .await?;
    Ok(Json(reaction.into()))
}

/// `DELETE {prefix}/v1/chats/{id}/messages/{msg_id}/reaction` — 204.
///
/// # Errors
/// Canonical 400 (path, non-assistant target), 403, 404, 500.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    st.reactions.remove(&ctx, chat_id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
