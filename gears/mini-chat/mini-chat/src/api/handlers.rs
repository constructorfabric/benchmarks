//! REST / SSE handlers.

use std::sync::Arc;

use axum::Extension;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::*;
use toolkit::api::odata::OData;
use toolkit::api::rest::extract;
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::sse::sse_response;
use crate::domain::error::DomainError;
use crate::domain::service::Core;
use crate::domain::service::stream::{SendRequest, StreamStart};

type CoreExt = Extension<Arc<Core>>;

fn joined(e: &tokio::task::JoinError) -> CanonicalError {
    DomainError::internal(format!("setup task failed: {e}")).into()
}

/// Creates a chat (`POST /chats`).
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<Response> {
    let chat = core.create_chat(&ctx, body.title, body.model).await?;
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), chat.id);
    let dto = ChatDetailDto::from(chat);
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        Json(dto),
    )
        .into_response())
}

/// Lists chats visible to the caller (`GET /chats`).
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = core.list_chats(&ctx, query).await?;
    Ok(Json(Page {
        items: page.items.into_iter().map(Into::into).collect(),
        page_info: page.page_info,
    }))
}

/// Returns a single chat (`GET /chats/{id}`).
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(core.get_chat(&ctx, id).await?.into()))
}

/// Renames a chat (`PATCH /chats/{id}`).
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(core.rename_chat(&ctx, id, &body.title).await?.into()))
}

/// Deletes a chat (`DELETE /chats/{id}`).
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<StatusCode> {
    core.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the messages of a chat (`GET /chats/{id}/messages`).
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = core.list_messages(&ctx, id, query).await?;
    Ok(Json(Page {
        items: page.items.into_iter().map(Into::into).collect(),
        page_info: page.page_info,
    }))
}

/// Runs the stream setup in its own task so a client disconnect does not interrupt it.
async fn run_setup<F>(fut: F) -> ApiResult<Response>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    let start = tokio::spawn(fut).await.map_err(|e| joined(&e))??;
    Ok(sse_response(start))
}

/// Sends a user message and streams the assistant reply over SSE.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let req = SendRequest {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    run_setup(async move { core.start_send(&ctx, id, req).await }).await
}

/// Returns the status of a turn.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(core.turn_status(&ctx, id, request_id).await?.into()))
}

/// Retries the latest turn and streams the new reply over SSE.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    run_setup(async move { core.mutate_and_stream(&ctx, id, request_id, None).await }).await
}

/// Edits the latest turn and streams the new reply over SSE.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    run_setup(async move {
        core.mutate_and_stream(&ctx, id, request_id, Some(body.content))
            .await
    })
    .await
}

/// Deletes the latest turn.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    core.delete_turn(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Sets the caller's reaction on an assistant message.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    Ok(Json(
        core.set_reaction(&ctx, id, msg_id, &body.reaction)
            .await?
            .into(),
    ))
}

/// Removes the caller's reaction from an assistant message.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    core.delete_reaction(&ctx, id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Lists the models available to the caller.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
) -> ApiResult<Json<ModelListDto>> {
    let items = core
        .list_models(&ctx)
        .await?
        .into_iter()
        .map(ModelDto::from)
        .collect();
    Ok(Json(ModelListDto { items }))
}

/// Returns a single model available to the caller.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    Ok(Json(core.get_model(&ctx, &id).await?.into()))
}

/// Returns the caller's quota status per period.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): CoreExt,
) -> ApiResult<Json<QuotaStatusResponse>> {
    let periods = core.quota_status(&ctx).await?;
    Ok(Json(QuotaStatusResponse::from_periods(
        periods,
        core.cfg.quota.warning_threshold_pct,
    )))
}
