//! Turn status handler (OWNER: REST CRUD work package).

use std::sync::Arc;

use axum::Extension;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::service::Services;

/// `GET /v1/chats/{id}/turns/{request_id}`.
pub async fn get_turn(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let dto = svc.turn_status.get(&ctx, chat_id, request_id).await?;
    Ok(axum::Json(dto).into_response())
}
