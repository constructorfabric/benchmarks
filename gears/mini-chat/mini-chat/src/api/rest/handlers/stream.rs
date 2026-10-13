//! `POST /v1/chats/{id}/messages:stream` (DESIGN §3.3 "Streaming Contract").

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::StreamMessageRequest;
use crate::api::rest::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::services::stream::live_events;
use crate::domain::services::{SendRequest, Services, StreamStart};

impl From<StreamMessageRequest> for SendRequest {
    fn from(r: StreamMessageRequest) -> Self {
        Self {
            content: r.content,
            request_id: r.request_id,
            attachment_ids: r.attachment_ids,
            web_search: r.web_search.is_some_and(|w| w.enabled),
        }
    }
}

/// Send a message and stream the answer as SSE.
///
/// The setup runs in its own task, awaited here: a client that goes away
/// during the setup does not interrupt it, and a turn committed meanwhile is
/// cancelled when its unsent stream is dropped (DESIGN §3.3 close rule 2).
/// Failures before the stream opens are canonical JSON problems.
///
/// # Errors
/// Canonical problem for validation, authorization, conflict, quota and setup
/// failures.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let stream = std::sync::Arc::clone(&svc.stream);
    let ping = stream.ping_interval();
    let setup = tokio::spawn(async move { stream.send(ctx, id, req.into()).await });
    let start = setup
        .await
        .map_err(|e| DomainError::internal(format!("stream setup task failed: {e}")))??;
    Ok(match start {
        StreamStart::Live(live) => sse_response(live_events(live, ping)),
        StreamStart::Replay(events) => sse_response(futures::stream::iter(events)),
    })
}
