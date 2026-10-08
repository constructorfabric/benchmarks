//! Send-message (SSE) handler.

use super::prelude::*;

/// `POST /v1/chats/{id}/messages:stream` — send a message and stream the answer.
///
/// # Errors
/// Returns the canonical error mapped from the service `DomainError`.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<StreamMessageRequest>,
) -> Result<Response, CanonicalError> {
    let input = SendInput {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    detached(async move { svc.send_message(&ctx, id, input).await }).await
}
