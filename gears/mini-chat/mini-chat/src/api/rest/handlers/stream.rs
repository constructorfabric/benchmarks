//! Streaming handler (DESIGN section 3.3 "Streaming Contract", section 3.6
//! "Send Message with Streaming Response", "SSE stream close rules").

use axum::extract::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::RestState;
use crate::api::rest::dto::StreamMessageRequest;
use crate::api::rest::sse;
use crate::domain::stream::SendOptions;

/// `POST {prefix}/v1/chats/{id}/messages:stream` — sends a message and
/// streams the answer as SSE. The setup runs in a spawned task (a client
/// disconnect does not interrupt it); every setup rejection is a JSON
/// `Problem` before the stream opens.
///
/// # Errors
///
/// 400 (`EMPTY_CONTENT`, `invalid_attachment`, `INVALID_MODEL`,
/// `TOO_MANY_IMAGES`, `FEATURE_DISABLED`, `VISION_NOT_SUPPORTED`,
/// `INPUT_TOO_LONG`, `CONTEXT_BUDGET_EXCEEDED`, path, extractor), 403, 404
/// (chat), 409 (`request_id_conflict`, `turn_already_running`), 415 / 422
/// (extractor), 429 (`quota_exceeded`), 500, 503.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(chat_id): Path<Uuid>,
    Json(req): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let options = SendOptions {
        trace_id: current_trace_id(),
    };
    let started = sse::run_send(state.stream.clone(), ctx, chat_id, req.into(), options).await?;
    Ok(sse::relay(started, state.sse_ping_interval))
}

/// Trace id of the current request span, when tracing is active.
pub(crate) fn current_trace_id() -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;

    let context = opentelemetry::Context::current();
    let span = context.span();
    let span_context = span.span_context();
    span_context
        .is_valid()
        .then(|| span_context.trace_id().to_string())
}
