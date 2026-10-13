//! Handlers of `POST /v1/chats/{id}/messages:stream` and
//! `GET /v1/chats/{id}/turns/{request_id}`. Extraction order (earlier wins):
//! path, JSON body; the services then ask the PDP and read or write.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{KeepAlive, Sse};
use futures::StreamExt;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract::{Json as ReqJson, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::stream_dto::{
    EditTurnRequest, MiniChatSseEvent, StreamMessageRequest, TurnStatusResponse,
};
use crate::domain::streaming::{StreamService, TurnEvents};
use crate::domain::turn_mutations::TurnMutationService;
use crate::domain::turns::TurnsService;

/// Interval of the SSE comment line that keeps proxies from closing a quiet
/// connection (DESIGN 3.3 `event: ping`).
const COMMENT_KEEP_ALIVE: Duration = Duration::from_secs(30);

/// `POST {prefix}/v1/chats/{id}/messages:stream`
///
/// # Errors
/// Canonical `Problem` (JSON, no SSE stream is opened): 400 (`EMPTY_CONTENT`,
/// `INVALID_MODEL`, `INPUT_TOO_LONG`, `CONTEXT_BUDGET_EXCEEDED`,
/// `invalid_attachment`, `FEATURE_DISABLED`, `invalid_path_params`,
/// `json_syntax_error`), 403, 404, 409 (`request_id_conflict`,
/// `turn_already_running`), 415, 422, 503 when the PDP is unreachable, 500.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<StreamService>>,
    Path(id): Path<Uuid>,
    ReqJson(body): ReqJson<StreamMessageRequest>,
) -> ApiResult<impl IntoResponse> {
    let events = service.send_message(ctx, id, body.into()).await?;
    Ok(sse_response(events))
}

/// `GET {prefix}/v1/chats/{id}/turns/{request_id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`invalid_path_params`), 403, 404 (chat or turn),
/// 503 when the PDP is unreachable, 500.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<TurnsService>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(service.get_turn(&ctx, id, request_id).await?.into()))
}

/// The SSE answer of a turn (`messages:stream`, retry, edit): the same
/// contract for all three.
fn sse_response(events: TurnEvents) -> impl IntoResponse {
    let stream =
        events.map(|event| Ok::<_, Infallible>(MiniChatSseEvent::from(event).into_sse_event()));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(COMMENT_KEEP_ALIVE))
}

/// `POST {prefix}/v1/chats/{id}/turns/{request_id}/retry`
///
/// # Errors
/// Canonical `Problem` (JSON, no SSE stream is opened): 400
/// (`failed_precondition` `turn_state`, kill switch, attachments, context
/// budget, `invalid_path_params`), 403, 404, 409 (`NOT_LATEST_TURN`,
/// `GENERATION_IN_PROGRESS`), 429, 503 when the PDP is unreachable, 500.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<TurnMutationService>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    Ok(sse_response(service.retry(ctx, id, request_id).await?))
}

/// `PATCH {prefix}/v1/chats/{id}/turns/{request_id}`
///
/// # Errors
/// As [`retry_turn`], plus 400 `EMPTY_CONTENT`, 415 and 422 for the body.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<TurnMutationService>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
    ReqJson(body): ReqJson<EditTurnRequest>,
) -> ApiResult<impl IntoResponse> {
    Ok(sse_response(
        service.edit(ctx, id, request_id, body.content).await?,
    ))
}

/// `DELETE {prefix}/v1/chats/{id}/turns/{request_id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`failed_precondition` `turn_state`,
/// `invalid_path_params`), 403, 404, 409 (`NOT_LATEST_TURN`), 503 when the PDP
/// is unreachable, 500.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<TurnMutationService>>,
    Path((id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    service.delete(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
