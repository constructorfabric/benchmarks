//! Turn mutation handlers (DESIGN section 3.9 "Turn Mutation API
//! Contracts"): retry and edit stream the replacement turn exactly like
//! `messages:stream` (same spawned setup and SSE relay); delete answers 204.

use axum::extract::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::{ApiResult, StatusCode};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::stream::current_trace_id;
use crate::api::rest::RestState;
use crate::api::rest::dto::EditTurnRequest;
use crate::api::rest::sse;
use crate::domain::stream::SendOptions;

/// `POST {prefix}/v1/chats/{id}/turns/{request_id}/retry` — retries the
/// latest turn and streams the new answer as SSE (no request body). Every
/// rejection before the stream opens is a JSON `Problem`.
///
/// # Errors
///
/// 400 (`turn_state`, `INVALID_MODEL`, `TOO_MANY_IMAGES`, `FEATURE_DISABLED`,
/// `VISION_NOT_SUPPORTED`, `INPUT_TOO_LONG`, `CONTEXT_BUDGET_EXCEEDED`,
/// path), 403, 404 (chat, turn), 409 (`NOT_LATEST_TURN`,
/// `GENERATION_IN_PROGRESS`), 429 (`quota_exceeded`), 500, 503.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let options = SendOptions {
        trace_id: current_trace_id(),
    };
    let turns = state.turns.clone();
    let started =
        sse::run_setup(async move { turns.retry(ctx, chat_id, request_id, options).await }).await?;
    Ok(sse::relay(started, state.sse_ping_interval))
}

/// `PATCH {prefix}/v1/chats/{id}/turns/{request_id}` — replaces the latest
/// turn's user message and streams the new answer as SSE.
///
/// # Errors
///
/// The errors of [`retry_turn`], plus 400 `EMPTY_CONTENT` and the JSON
/// extractor rejections (400 / 415 / 422).
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let options = SendOptions {
        trace_id: current_trace_id(),
    };
    let turns = state.turns.clone();
    let started = sse::run_setup(async move {
        turns
            .edit(ctx, chat_id, request_id, req.content, options)
            .await
    })
    .await?;
    Ok(sse::relay(started, state.sse_ping_interval))
}

/// `DELETE {prefix}/v1/chats/{id}/turns/{request_id}` — soft-deletes the
/// latest turn, 204.
///
/// # Errors
///
/// 400 (`turn_state`, path), 403, 404 (chat, turn), 409 (`NOT_LATEST_TURN`),
/// 500, 503.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state.turns.delete(&ctx, chat_id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
