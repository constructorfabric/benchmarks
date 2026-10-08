//! Turn handlers: status (DESIGN section 3.3, Turn Status API) and the
//! last-turn mutations retry, edit and delete (section 3.9).

use std::future::Future;
use std::sync::Arc;

use axum::Extension;
use axum::http::StatusCode;
use axum::response::Response;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{EditTurnRequest, TurnStatusResponse};
use crate::api::rest::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::services::stream::StreamStart;
use crate::gear::AppState;

/// `GET {prefix}/v1/chats/{id}/turns/{request_id}`
///
/// # Errors
/// Canonical 400 (path), 403, 404 (chat or turn), 500.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(
        st.turns.status(&ctx, chat_id, request_id).await?.into(),
    ))
}

/// Runs a stream setup in a spawned task the handler awaits (DESIGN section
/// 3.3, close rule 2: a client disconnect during setup does not abort it),
/// then answers with the SSE stream (same contract as `messages:stream`).
async fn spawned_sse<F>(setup: F) -> ApiResult<Response>
where
    F: Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    let start = tokio::spawn(setup)
        .await
        .map_err(|e| DomainError::Internal(format!("stream setup task failed: {e}")))??;
    Ok(sse_response(start))
}

/// `POST {prefix}/v1/chats/{id}/turns/{request_id}/retry` (no body): SSE of
/// the replacement turn.
///
/// # Errors
/// Canonical 400/403/404/409/429/500/503 (pre-stream).
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let svc = Arc::clone(&st.stream);
    spawned_sse(async move { svc.retry(ctx, chat_id, request_id).await }).await
}

/// `PATCH {prefix}/v1/chats/{id}/turns/{request_id}`: SSE of the replacement
/// turn with the new content.
///
/// # Errors
/// Canonical 400/403/404/409/415/422/429/500/503 (pre-stream).
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let svc = Arc::clone(&st.stream);
    spawned_sse(async move { svc.edit(ctx, chat_id, request_id, body.content).await }).await
}

/// `DELETE {prefix}/v1/chats/{id}/turns/{request_id}` — 204.
///
/// # Errors
/// Canonical 400/403/404/409/500/503.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    st.turns.delete(&ctx, chat_id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
