//! Turn handlers: `mini_chat.get_turn`, `mini_chat.retry_turn`, `mini_chat.edit_turn`,
//! `mini_chat.delete_turn`.

use std::sync::Arc;

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use tracing::Instrument as _;
use uuid::Uuid;

use crate::api::dto::turns::{EditTurnRequest, TurnStatusResponse};
use crate::api::sse::into_sse_response;
use crate::api::state::AppServices;
use crate::domain::error::DomainError;
use crate::domain::stream::EventStream;

/// `mini_chat.get_turn`: the state of the turn of `request_id`.
///
/// # Errors
/// 404 for a missing chat or a missing / deleted turn, 403 / 503 from the PDP.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(
        svc.turns.status(&ctx, chat_id, request_id).await?.into(),
    ))
}

/// `mini_chat.retry_turn`: replaces the latest turn with a new generation of its user message
/// and streams it as SSE.
///
/// # Errors
/// Every rejection before the stream as a JSON problem (400, 403, 404, 409, 429, 500, 503).
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let turns = Arc::clone(&svc.turns);
    stream_detached(async move { turns.retry(&ctx, chat_id, request_id).await }).await
}

/// `mini_chat.edit_turn`: replaces the latest turn with a generation of the new content and
/// streams it as SSE.
///
/// # Errors
/// Like [`retry_turn`], plus 400 `EMPTY_CONTENT`.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let turns = Arc::clone(&svc.turns);
    stream_detached(async move { turns.edit(&ctx, chat_id, request_id, req.content).await }).await
}

/// `mini_chat.delete_turn`: soft-deletes the latest turn.
///
/// # Errors
/// 400 (running turn), 403, 404, 409 (not the latest turn), 500, 503.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.turns.delete(&ctx, chat_id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Runs a mutation setup in a spawned task and awaits it, so a turn committed before a client
/// disconnect still gets its provider task (as `messages:stream`, DESIGN 1163).
async fn stream_detached<F>(setup: F) -> ApiResult<Response>
where
    F: Future<Output = Result<EventStream, DomainError>> + Send + 'static,
{
    let events = tokio::spawn(setup.in_current_span())
        .await
        .map_err(|e| DomainError::Internal(format!("turn mutation task failed: {e}")))??;
    Ok(into_sse_response(events))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use crate::test_support::app::{TestApp, TestResponse, ctx};
    use crate::test_support::gateway::{Responder, SseScript};
    use crate::test_support::stream::{
        CHATS, answer, create_chat, failed, script_provider, stream_uri, text_delta, turn_of,
    };

    const TURN_RESOURCE: &str = "gts.cf.core.mini_chat.turn.v1~";

    fn user() -> SecurityContext {
        ctx(Uuid::new_v4(), Uuid::new_v4())
    }

    async fn status(
        app: &TestApp,
        who: &SecurityContext,
        chat: Uuid,
        request: impl std::fmt::Display,
    ) -> TestResponse {
        app.call("GET", &format!("{CHATS}/{chat}/turns/{request}"), who, None)
            .await
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one scenario over every turn state
    async fn turn_status_states() {
        let app = TestApp::builder().build().await;
        let who = user();
        let chat = create_chat(&app, &who, None).await;

        // Completed.
        script_provider(&app, answer(&["Hi"], 3, 2));
        let done_id = Uuid::new_v4();
        app.stream(
            "POST",
            &stream_uri(chat),
            &who,
            json!({"content": "a", "request_id": done_id}),
        )
        .await
        .unwrap_or_else(|r| panic!("{}", r.json));
        let res = status(&app, &who, chat, done_id).await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["request_id"], json!(done_id));
        assert_eq!(res.json["state"], "done");
        let stored = turn_of(&app, chat, done_id).await;
        assert_eq!(
            res.json["assistant_message_id"],
            json!(stored.assistant_message_id.unwrap())
        );
        assert!(res.json.get("error_code").is_none(), "{}", res.json);
        assert_eq!(
            time::OffsetDateTime::parse(
                res.json["updated_at"].as_str().unwrap(),
                &time::format_description::well_known::Rfc3339
            )
            .unwrap(),
            crate::api::dto::timestamp::to_api(stored.updated_at)
        );

        // Failed.
        app.gateway.clear_rules();
        script_provider(&app, Responder::Sse(vec![failed("model overloaded")]));
        let failed_id = Uuid::new_v4();
        app.stream(
            "POST",
            &stream_uri(chat),
            &who,
            json!({"content": "b", "request_id": failed_id}),
        )
        .await
        .unwrap_or_else(|r| panic!("{}", r.json));
        let res = status(&app, &who, chat, failed_id).await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["state"], "error");
        assert_eq!(res.json["error_code"], "provider_error");
        assert!(
            res.json.get("assistant_message_id").is_none(),
            "{}",
            res.json
        );

        // Running: the provider never finishes.
        app.gateway.clear_rules();
        script_provider(
            &app,
            Responder::Sse(vec![text_delta("par"), SseScript::Hang]),
        );
        let running_id = Uuid::new_v4();
        let mut reader = app
            .open_stream(
                "POST",
                &stream_uri(chat),
                &who,
                json!({"content": "c", "request_id": running_id}),
            )
            .await
            .unwrap_or_else(|r| panic!("{}", r.json));
        reader.next_frame().await.expect("stream_started");
        reader.next_frame().await.expect("the partial delta");
        let res = status(&app, &who, chat, running_id).await;
        assert_eq!(res.status, 200, "{}", res.json);
        assert_eq!(res.json["state"], "running");
        assert!(
            res.json.get("assistant_message_id").is_none(),
            "{}",
            res.json
        );
        assert!(res.json.get("error_code").is_none(), "{}", res.json);

        // Cancelled by a client disconnect; the partial text was persisted.
        drop(reader);
        TestApp::wait_until("the disconnect cancels the turn", || async {
            status(&app, &who, chat, running_id).await.json["state"] == "cancelled"
        })
        .await;
        let res = status(&app, &who, chat, running_id).await;
        assert!(res.json.get("error_code").is_none(), "{}", res.json);
        let cancelled = turn_of(&app, chat, running_id).await;
        assert_eq!(
            res.json["assistant_message_id"],
            json!(
                cancelled
                    .assistant_message_id
                    .expect("partial text persisted")
            ),
            "{}",
            res.json
        );

        let unknown = status(&app, &who, chat, Uuid::new_v4()).await;
        assert_eq!(unknown.status, 404, "{}", unknown.json);
        assert_eq!(unknown.json["context"]["resource_type"], TURN_RESOURCE);
        let foreign = status(&app, &user(), chat, done_id).await;
        assert_eq!(foreign.status, 404, "{}", foreign.json);
        let bad = status(&app, &who, chat, "not-a-uuid").await;
        assert_eq!(bad.status, 400, "{}", bad.json);
        assert_eq!(
            bad.json["context"]["field_violations"][0]["reason"],
            "invalid_path_params"
        );
    }
}
