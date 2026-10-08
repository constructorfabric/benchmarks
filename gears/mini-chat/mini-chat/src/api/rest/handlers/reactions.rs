//! Reaction handlers (OWNER: REST CRUD work package).

use std::sync::Arc;

use axum::Extension;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::SetReactionReq;
use crate::domain::service::Services;

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction` → 200.
pub async fn put_reaction(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, msg_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetReactionReq>,
) -> ApiResult<Response> {
    let dto = svc
        .reactions
        .set(&ctx, chat_id, msg_id, &req.reaction)
        .await?;
    Ok(axum::Json(dto).into_response())
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction` → 204.
pub async fn delete_reaction(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.reactions.delete(&ctx, chat_id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
