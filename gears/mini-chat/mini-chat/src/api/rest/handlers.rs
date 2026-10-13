//! REST handlers.

use std::sync::Arc;

use axum::Extension;
use axum::http::Uri;
use axum::response::Response;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_canonical_errors::CanonicalError;
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
use crate::domain::service::MiniChat;
use crate::domain::stream::StreamStart;
use crate::domain::stream::send::SendRequest;
use crate::domain::turn_service::Mutation;

type Svc = Extension<Arc<MiniChat>>;

fn join_err(e: &tokio::task::JoinError) -> CanonicalError {
    CanonicalError::internal(format!("stream setup task failed: {e}")).create()
}

/// Run a stream setup in a separate task (a client disconnect while waiting
/// does not interrupt it; an unsent live stream is cancelled when dropped).
async fn spawn_setup<F>(fut: F) -> ApiResult<StreamStart>
where
    F: std::future::Future<Output = Result<StreamStart, DomainError>> + Send + 'static,
{
    match tokio::spawn(fut).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(e.into()),
        Err(e) => Err(join_err(&e)),
    }
}

/// Create a chat (`POST /chats`).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn create_chat(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let chat = svc.create_chat(&ctx, body.title, body.model).await?;
    let id = chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(chat), &uri, &id))
}

/// List chats visible to the caller (`GET /chats`).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    OData(query): OData,
) -> ApiResult<Json<Page<ChatDetailDto>>> {
    let page = svc.list_chats(&ctx, &query).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// Get a chat by id (`GET /chats/{id}`).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.get_chat(&ctx, id).await?.into()))
}

/// Rename a chat (`PATCH /chats/{id}`).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.update_chat(&ctx, id, &body.title).await?.into()))
}

/// Delete a chat (`DELETE /chats/{id}`).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<StatusCode> {
    svc.delete_chat(&ctx, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the messages of a chat (`GET /chats/{id}/messages`).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    OData(query): OData,
) -> ApiResult<Json<Page<MiniChatMessageDto>>> {
    let page = svc.list_messages(&ctx, id, &query).await?;
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}

/// Send a message and stream the assistant reply as SSE.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<StreamMessageRequest>,
) -> ApiResult<Response> {
    let req = SendRequest {
        content: body.content,
        request_id: body.request_id,
        attachment_ids: body.attachment_ids.unwrap_or_default(),
        web_search: body.web_search.is_some_and(|w| w.enabled),
    };
    let start = spawn_setup(async move { svc.start_send(&ctx, id, req).await }).await?;
    Ok(sse_response(start))
}

/// Get the status of a turn.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(svc.get_turn(&ctx, id, request_id).await?.into()))
}

/// Retry a turn and stream the new reply as SSE.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let start = spawn_setup(async move { svc.start_mutation(&ctx, id, request_id, Mutation::Retry).await }).await?;
    Ok(sse_response(start))
}

/// Edit a turn and stream the new reply as SSE.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let start = spawn_setup(async move {
        svc.start_mutation(&ctx, id, request_id, Mutation::Edit(body.content))
            .await
    })
    .await?;
    Ok(sse_response(start))
}

/// Delete a turn.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_turn(&ctx, id, request_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Set the caller's reaction on a message.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    Ok(Json(svc.set_reaction(&ctx, id, msg_id, &body.reaction).await?.into()))
}

/// Remove the caller's reaction from a message.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_reaction(&ctx, id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// List the models available to the caller.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
) -> ApiResult<Json<ModelListDto>> {
    let items = svc.list_models(&ctx).await?.into_iter().map(ModelDto::from).collect();
    Ok(Json(ModelListDto { items }))
}

/// Get a model by id.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    Ok(Json(svc.get_model(&ctx, &id).await?.into()))
}

/// Get the caller's quota status.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Svc,
) -> ApiResult<Json<QuotaStatusResponse>> {
    Ok(Json(svc.quota_status(&ctx).await?.into()))
}
