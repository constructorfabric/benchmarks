//! Turn handlers (DESIGN §3.3 "Turn Status API", §3.9 "Turn Mutation API
//! Contracts").

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{EditTurnRequest, TurnStatusResponse};
use crate::api::rest::sse::sse_response;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::services::Services;
use crate::domain::services::stream::{LiveStream, live_events};

/// `GET /v1/chats/{id}/turns/{request_id}`.
///
/// # Errors
/// Canonical problem for authorization failures and unknown chats or turns.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let turn = svc.turns.status(&ctx, id, request_id).await?;
    Ok(Json(TurnStatusResponse::try_from(turn)?))
}

/// Run a retry/edit setup in its own task (a client that goes away during the
/// setup does not interrupt the mutation; a committed turn whose stream is never
/// sent is cancelled when the stream is dropped) and answer with its SSE stream.
async fn stream_mutation<F>(svc: &Services, setup: F) -> ApiResult<Response>
where
    F: Future<Output = DomainResult<LiveStream>> + Send + 'static,
{
    let ping = svc.stream.ping_interval();
    let live = tokio::spawn(setup)
        .await
        .map_err(|e| DomainError::internal(format!("turn mutation setup task failed: {e}")))??;
    Ok(sse_response(live_events(live, ping)))
}

/// `POST /v1/chats/{id}/turns/{request_id}/retry`: SSE stream of the new turn.
///
/// # Errors
/// Canonical problem for authorization, target, preflight and setup failures.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let turns = std::sync::Arc::clone(&svc.turns);
    stream_mutation(&svc, async move { turns.retry(&ctx, id, request_id).await }).await
}

/// `PATCH /v1/chats/{id}/turns/{request_id}`: SSE stream of the edited turn.
///
/// # Errors
/// Canonical problem for validation, authorization, target, preflight and setup
/// failures.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let turns = std::sync::Arc::clone(&svc.turns);
    stream_mutation(&svc, async move {
        turns.edit(&ctx, id, request_id, req.content).await
    })
    .await
}

/// `DELETE /v1/chats/{id}/turns/{request_id}`: 204 No Content.
///
/// # Errors
/// Canonical problem for authorization and target failures.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.turns.delete(&ctx, id, request_id).await?;
    Ok(no_content())
}
