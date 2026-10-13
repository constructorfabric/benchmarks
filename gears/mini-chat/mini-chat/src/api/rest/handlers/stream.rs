//! `POST /v1/chats/{id}/messages:stream` — send a message, stream the answer.

use std::sync::Arc;

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::StreamMessageRequest;
use crate::api::rest::sse::into_sse;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::domain::services::stream_service::SendMessage;

/// 200 `text/event-stream`; every rejection before the stream opens is a
/// JSON `Problem`. The setup runs in a spawned task the handler awaits, so a
/// client disconnect during setup does not abort a committed turn (its
/// unsent stream is dropped, which cancels the turn).
///
/// # Errors
/// 400 / 403 / 404 / 409 / 415 / 422 / 429 / 500 / 503 per ADR-0004.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    Json(req): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let send = SendMessage {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids,
        web_search_enabled: req.web_search.is_some_and(|w| w.enabled),
    };
    let streams = Arc::clone(&svc.streams);
    let stream = tokio::spawn(async move { streams.send(ctx, id, send).await })
        .await
        .map_err(|e| DomainError::Internal(format!("send setup task failed: {e}")))??;
    Ok(into_sse(stream, svc.sse_ping_interval))
}
