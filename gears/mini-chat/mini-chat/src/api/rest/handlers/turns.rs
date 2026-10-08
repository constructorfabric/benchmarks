//! Turns: `GET /v1/chats/{id}/turns/{request_id}` (status),
//! `POST .../turns/{request_id}/retry`, `PATCH .../turns/{request_id}`
//! (edit) and `DELETE .../turns/{request_id}`.

use std::sync::Arc;

use axum::Extension;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::{ApiResult, no_content};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{EditTurnRequest, TurnStatusResponse};
use crate::api::rest::sse::into_sse;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::domain::services::stream_service::TurnStream;

/// Authoritative turn state (`running` / `done` / `error` / `cancelled`).
///
/// # Errors
/// 404 (`chat` / `turn`), 400 (non-UUID path), 403 / 503 from
/// authorization, 500.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let status = svc.turns.status(&ctx, id, request_id).await?;
    Ok(Json(status.into()))
}

/// Retry the latest turn; 200 `text/event-stream` (same contract as
/// `messages:stream`). Every rejection before the stream opens is a JSON
/// `Problem`. The setup runs in a spawned task the handler awaits, so a
/// client disconnect does not abort a committed mutation.
///
/// # Errors
/// 400 / 403 / 404 / 409 / 429 / 500 / 503 per D§3.9.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let turns = Arc::clone(&svc.turns);
    let live = tokio::spawn(async move { turns.retry(ctx, id, request_id).await })
        .await
        .map_err(|e| DomainError::Internal(format!("retry setup task failed: {e}")))??;
    Ok(into_sse(TurnStream::Live(live), svc.sse_ping_interval))
}

/// Edit the latest turn's user message; 200 `text/event-stream` (as
/// [`retry_turn`]).
///
/// # Errors
/// As [`retry_turn`], plus 400 `EMPTY_CONTENT` and 422 for a bad body.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let turns = Arc::clone(&svc.turns);
    let live = tokio::spawn(async move { turns.edit(ctx, id, request_id, req.content).await })
        .await
        .map_err(|e| DomainError::Internal(format!("edit setup task failed: {e}")))??;
    Ok(into_sse(TurnStream::Live(live), svc.sse_ping_interval))
}

/// Delete the latest turn (204, no body).
///
/// # Errors
/// 400 / 403 / 404 / 409 / 500 / 503 per D§3.9.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.turns.delete(&ctx, id, request_id).await?;
    Ok(no_content())
}
