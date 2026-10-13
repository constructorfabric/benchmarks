//! Handlers (OWNER: streaming core work package): send message, retry / edit / delete turn.
//!
//! The stream setup runs in a spawned task that the handler awaits, so a client disconnect
//! before the stream opens does not abort a committed setup: the dropped response cancels the
//! turn instead (DESIGN "SSE stream close rules", rule 2).

use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::stream::{self, BoxStream, StreamExt};
use http::StatusCode;
use http::header::{CACHE_CONTROL, HeaderValue};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{EditTurnRequest, MiniChatSseEvent, StreamMessageRequest};
use crate::domain::error::DomainError;
use crate::domain::service::Services;
use crate::domain::service::stream::{SendInput, StreamStart};

/// SSE comment keep-alive interval (hardcoded, B.4).
const KEEP_ALIVE: Duration = Duration::from_secs(30);

fn to_event(ev: &MiniChatSseEvent) -> Event {
    Event::default()
        .event(ev.name())
        .json_data(ev.data_json())
        .unwrap_or_else(|_| Event::default().event(ev.name()).data("{}"))
}

/// Converts a stream setup into a `text/event-stream` response.
#[must_use]
pub fn sse_response(start: StreamStart) -> Response {
    let events: BoxStream<'static, MiniChatSseEvent> = match start {
        StreamStart::Replay(evs) => stream::iter(evs).boxed(),
        StreamStart::Live(live) => live.into_events().boxed(),
    };
    let body = events.map(|ev| Ok::<Event, Infallible>(to_event(&ev)));
    let mut resp = Sse::new(body)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response();
    resp.headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

/// Runs a setup future in its own task and awaits it.
async fn detached<F>(fut: F) -> Result<StreamStart, DomainError>
where
    F: Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    tokio::spawn(fut)
        .await
        .map_err(|e| DomainError::internal(format!("stream setup task failed: {e}")))?
}

pub async fn stream_message(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(chat_id): Path<Uuid>,
    Json(body): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let input = SendInput {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    let stream_svc = Arc::clone(&svc.stream);
    let start = detached(async move { stream_svc.send(&ctx, chat_id, input).await }).await?;
    Ok(sse_response(start))
}

pub async fn edit_turn(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let m = Arc::clone(&svc.mutations);
    let start =
        detached(async move { m.edit(&ctx, chat_id, request_id, body.content).await }).await?;
    Ok(sse_response(start))
}

pub async fn retry_turn(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let m = Arc::clone(&svc.mutations);
    let start = detached(async move { m.retry(&ctx, chat_id, request_id).await }).await?;
    Ok(sse_response(start))
}

pub async fn delete_turn(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.mutations.delete(&ctx, chat_id, request_id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
