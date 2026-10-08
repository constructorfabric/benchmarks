//! REST handlers.

use std::sync::Arc;

use axum::Extension;
use axum::response::Response;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::sse::sse_response;
use crate::domain::service::MiniChat;
use crate::domain::stream::SendRequest;
use crate::domain::turns::MutationKind;

type Svc = Extension<Arc<MiniChat>>;
type Ctx = Extension<SecurityContext>;

/// Create a new chat.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn create_chat(
    uri: axum::http::Uri,
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Json(body): extract::Json<CreateChatReq>,
) -> ApiResult<Response> {
    let view = svc.create_chat(&ctx, body.title, body.model).await?;
    let id = view.chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(view), &uri, &id).into_response())
}

/// List chats of the current user.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn list_chats(Extension(ctx): Ctx, Extension(svc): Svc, odata: OData) -> ApiResult<JsonPage<ChatDetailDto>> {
    let page = svc.list_chats(&ctx, odata.into_inner()).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// Get a chat by id.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn get_chat(Extension(ctx): Ctx, Extension(svc): Svc, extract::Path(id): extract::Path<Uuid>) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.get_chat(&ctx, id).await?.into()))
}

/// Update a chat title.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn update_chat(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(body): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    Ok(Json(svc.update_chat_title(&ctx, id, &body.title).await?.into()))
}

/// Delete a chat.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn delete_chat(Extension(ctx): Ctx, Extension(svc): Svc, extract::Path(id): extract::Path<Uuid>) -> ApiResult<Response> {
    svc.delete_chat(&ctx, id).await?;
    Ok(no_content().into_response())
}

/// List messages of a chat.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn list_messages(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    odata: OData,
) -> ApiResult<JsonPage<MiniChatMessageDto>> {
    let page = svc.list_messages(&ctx, id, odata.into_inner()).await?;
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}

/// Send a message and stream the assistant reply over SSE.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn stream_message(
    Extension(ctx): Ctx,
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
    let start = svc.send_message(&ctx, id, req).await?;
    Ok(sse_response(start))
}

/// Upload an attachment to a chat.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn upload_attachment(
    uri: axum::http::Uri,
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path(id): extract::Path<Uuid>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> ApiResult<Response> {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let a = svc.upload_attachment(&ctx, id, ct, body.into_data_stream()).await?;
    let aid = a.id.to_string();
    Ok(created_json(AttachmentDetailDto::from(a), &uri, &aid).into_response())
}

/// Get attachment details.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn get_attachment(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(svc.get_attachment(&ctx, id, attachment_id).await?.into()))
}

/// Delete an attachment.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn delete_attachment(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.delete_attachment(&ctx, id, attachment_id).await?;
    Ok(no_content().into_response())
}

/// Get turn status.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn get_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    Ok(Json(svc.get_turn(&ctx, id, request_id).await?.into()))
}

/// Retry a turn and stream the new reply over SSE.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn retry_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let start = svc.mutate_turn(&ctx, id, request_id, MutationKind::Retry, None).await?;
    Ok(sse_response(start))
}

/// Edit a turn and stream the new reply over SSE.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn edit_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<EditTurnRequest>,
) -> ApiResult<Response> {
    let start = svc.mutate_turn(&ctx, id, request_id, MutationKind::Edit, Some(body.content)).await?;
    Ok(sse_response(start))
}

/// Delete a turn.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn delete_turn(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.delete_turn(&ctx, id, request_id).await?;
    Ok(no_content().into_response())
}

/// Set a reaction on a message.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn put_reaction(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(body): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    Ok(Json(svc.set_reaction(&ctx, id, msg_id, &body.reaction).await?.into()))
}

/// Remove a reaction from a message.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn delete_reaction(
    Extension(ctx): Ctx,
    Extension(svc): Svc,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.delete_reaction(&ctx, id, msg_id).await?;
    Ok(no_content().into_response())
}

/// List models available to the current user.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn list_models(Extension(ctx): Ctx, Extension(svc): Svc) -> ApiResult<Json<ModelListDto>> {
    let items = svc.list_models(&ctx).await?.into_iter().map(ModelDto::from).collect();
    Ok(Json(ModelListDto { items }))
}

/// Get a model by id.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn get_model(Extension(ctx): Ctx, Extension(svc): Svc, extract::Path(id): extract::Path<String>) -> ApiResult<Json<ModelDto>> {
    Ok(Json(svc.get_model(&ctx, &id).await?.into()))
}

/// Get the quota status of the current user.
///
/// # Errors
/// Returns a canonical API error if the domain operation fails.
pub async fn get_quota_status(Extension(ctx): Ctx, Extension(svc): Svc) -> ApiResult<Json<QuotaStatusResponse>> {
    Ok(Json(svc.quota_status(&ctx).await?.into()))
}
