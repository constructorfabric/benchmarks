//! `messages:stream`, retry and edit: SSE responses (DESIGN §3.3 Streaming Contract).

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Extension;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use tokio::sync::mpsc;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{License, V1};
use crate::api::dto::{EditTurnRequest, MiniChatSseEvent, StreamMessageRequest};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::domain::stream::events::StreamEvent;
use crate::domain::stream::relay;
use crate::domain::stream::setup::{self, SendInput, StartOutcome};
use crate::domain::turns::{self, Mutation};

fn to_sse(ev: &StreamEvent) -> Event {
    Event::default().event(ev.name()).data(ev.data())
}

/// SSE body from the relay channel; synthesizes `stream_interrupted` when the provider task ends
/// without a terminal event.
fn live_stream(mut rx: mpsc::Receiver<StreamEvent>) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        let mut terminal = false;
        while let Some(ev) = rx.recv().await {
            terminal = ev.is_terminal();
            yield Ok(to_sse(&ev));
            if terminal {
                break;
            }
        }
        if !terminal {
            yield Ok(to_sse(&relay::stream_interrupted()));
        }
    }
}

fn sse_response(app: Arc<AppServices>, outcome: StartOutcome) -> Response {
    let keep_alive = KeepAlive::new().interval(Duration::from_secs(30));
    match outcome {
        StartOutcome::Replay(events) => {
            let s = futures::stream::iter(events.into_iter().map(|e| Ok::<_, Infallible>(to_sse(&e))));
            Sse::new(s).keep_alive(keep_alive).into_response()
        }
        StartOutcome::Live(live) => {
            let rx = relay::spawn(app, *live);
            Sse::new(live_stream(rx)).keep_alive(keep_alive).into_response()
        }
    }
}

/// Runs the setup in a separate task so a client disconnect does not interrupt it.
async fn run_setup<F>(fut: F) -> Result<StartOutcome, DomainError>
where
    F: std::future::Future<Output = Result<StartOutcome, DomainError>> + Send + 'static,
{
    tokio::spawn(fut).await.map_err(|e| DomainError::internal(format!("stream setup task failed: {e}")))?
}

pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(chat_id): Path<Uuid>,
    Json(body): Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let input = SendInput {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids,
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    let app2 = Arc::clone(&app);
    let outcome = run_setup(async move { setup::start_send(&app2, &ctx, chat_id, input).await }).await?;
    Ok(sse_response(app, outcome))
}

pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let app2 = Arc::clone(&app);
    let outcome = run_setup(async move { turns::start_mutation(&app2, &ctx, chat_id, request_id, Mutation::Retry).await }).await?;
    Ok(sse_response(app, outcome))
}

pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let app2 = Arc::clone(&app);
    let outcome = run_setup(async move {
        turns::start_mutation(&app2, &ctx, chat_id, request_id, Mutation::Edit { content: body.content }).await
    })
    .await?;
    Ok(sse_response(app, outcome))
}

/// Registers the streaming routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = OperationBuilder::post(format!("{V1}/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response (SSE)")
        .tag("Mini Chat")
        .path_param("id", "Chat id")
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat response events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);
    let router = OperationBuilder::post(format!("{V1}/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest turn (SSE)")
        .tag("Mini Chat")
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);
    OperationBuilder::patch(format!("{V1}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn (SSE)")
        .tag("Mini Chat")
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .json_request::<EditTurnRequest>(openapi, "Replacement user message")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

