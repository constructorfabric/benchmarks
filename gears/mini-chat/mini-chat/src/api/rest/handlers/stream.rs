//! `messages:stream` handler (DESIGN section 3.3, "Streaming Contract").

use std::sync::Arc;

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::StreamMessageRequest;
use crate::api::rest::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::services::stream::SendMessage;
use crate::gear::AppState;

/// `POST {prefix}/v1/chats/{id}/messages:stream`
///
/// The setup runs in a spawned task that the handler awaits (DESIGN section
/// 3.3, close rule 2): a client that disconnects during the setup does not
/// abort it, so a committed turn always gets its provider task. Pre-stream
/// failures are canonical `Problem`s; afterwards the response is SSE (live or
/// replay). The LLM is never awaited before the response is returned.
///
/// # Errors
/// Canonical 400/403/404/409/415/422/429/500/503 (pre-stream).
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(chat_id): Path<Uuid>,
    Json(body): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let req = SendMessage {
        chat_id,
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids,
        web_search_enabled: body.web_search.is_some_and(|w| w.enabled),
    };
    let svc = Arc::clone(&st.stream);
    let start = tokio::spawn(async move { svc.send(ctx, req).await })
        .await
        .map_err(|e| DomainError::Internal(format!("stream setup task failed: {e}")))??;
    Ok(sse_response(start))
}
