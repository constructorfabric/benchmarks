//! Turn status and mutation handlers.

use super::prelude::*;

/// `GET /v1/chats/{id}/turns/{request_id}` — turn status.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(svc.turn_status(&ctx, id, request_id).await.map_err(err)?.into()))
}

/// `POST /v1/chats/{id}/turns/{request_id}/retry` — retry the last turn.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> Result<Response, CanonicalError> {
    detached(async move { svc.mutate_turn(&ctx, id, request_id, None).await }).await
}

/// `PATCH /v1/chats/{id}/turns/{request_id}` — edit the last turn.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<EditTurnRequest>,
) -> Result<Response, CanonicalError> {
    detached(async move { svc.mutate_turn(&ctx, id, request_id, Some(body.content)).await }).await
}

/// `DELETE /v1/chats/{id}/turns/{request_id}` — delete the last turn.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_turn(&ctx, id, request_id).await.map_err(err)?;
    Ok(StatusCode::NO_CONTENT)
}
