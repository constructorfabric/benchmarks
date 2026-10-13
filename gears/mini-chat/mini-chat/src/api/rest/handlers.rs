//! REST handlers: thin adapters between HTTP and the domain services.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use toolkit::api::canonical_prelude::{ApiResult, Json, JsonBody, OData, created_json, no_content};
use toolkit::api::rest::extract::{Json as RequestJson, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use tracing::Instrument;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatRequest, EditTurnRequest, MessageDto, ModelDto,
    ModelListResponse, QuotaStatusDto, ReactionDto, SendMessageRequest, SetReactionRequest,
    TurnStatusDto, UpdateChatRequest,
};
use super::sse::sse_response;
use super::upload::read_file;
use crate::domain::attachment::AttachmentService;
use crate::domain::chat::ChatService;
use crate::domain::error::DomainError;
use crate::domain::message::MessageService;
use crate::domain::models::ModelsService;
use crate::domain::mutation::TurnMutationService;
use crate::domain::quota::QuotaStatusService;
use crate::domain::stream::{SendInput, StreamService};
use crate::domain::turn::TurnService;

/// `POST /v1/chats`: creates a chat; 201 with a `Location` header.
pub(crate) async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatService>>,
    uri: Uri,
    RequestJson(request): RequestJson<CreateChatRequest>,
) -> ApiResult<impl IntoResponse> {
    let detail = chats
        .create_chat(&ctx, request.title.as_deref(), request.model.as_deref())
        .await?;
    let id = detail.chat.id.to_string();
    Ok(created_json(ChatDetailDto::from(detail), &uri, &id))
}

/// `GET /v1/chats`: lists the caller's chats (`OData` filter, order and cursor pagination).
pub(crate) async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatService>>,
    OData(query): OData,
) -> ApiResult<JsonBody<Page<ChatDetailDto>>> {
    let page = chats.list_chats(&ctx, &query).await?;
    Ok(Json(page.map_items(ChatDetailDto::from)))
}

/// `GET /v1/chats/{id}`.
pub(crate) async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatService>>,
    Path(chat_id): Path<Uuid>,
) -> ApiResult<JsonBody<ChatDetailDto>> {
    let detail = chats.get_chat(&ctx, chat_id).await?;
    Ok(Json(ChatDetailDto::from(detail)))
}

/// `PATCH /v1/chats/{id}`: renames the chat.
pub(crate) async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatService>>,
    Path(chat_id): Path<Uuid>,
    RequestJson(request): RequestJson<UpdateChatRequest>,
) -> ApiResult<JsonBody<ChatDetailDto>> {
    let detail = chats.update_title(&ctx, chat_id, &request.title).await?;
    Ok(Json(ChatDetailDto::from(detail)))
}

/// `DELETE /v1/chats/{id}`: soft-deletes the chat; 204.
pub(crate) async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(chats): Extension<Arc<ChatService>>,
    Path(chat_id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    chats.delete_chat(&ctx, chat_id).await?;
    Ok(no_content())
}

/// `GET /v1/models`.
pub(crate) async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(models): Extension<Arc<ModelsService>>,
) -> ApiResult<JsonBody<ModelListResponse>> {
    let items = models.list_models(&ctx).await?;
    Ok(Json(ModelListResponse {
        items: items.into_iter().map(ModelDto::from).collect(),
    }))
}

/// `GET /v1/models/{id}`.
pub(crate) async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(models): Extension<Arc<ModelsService>>,
    Path(model_id): Path<String>,
) -> ApiResult<JsonBody<ModelDto>> {
    let model = models.get_model(&ctx, &model_id).await?;
    Ok(Json(ModelDto::from(model)))
}

/// `POST /v1/chats/{id}/messages:stream`: the setup runs in a spawned task that this handler
/// awaits, so a client that disconnects before the response is sent still gets its committed turn
/// finalized (the dropped response cancels the provider task).
pub(crate) async fn send_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(stream): Extension<Arc<StreamService>>,
    Path(chat_id): Path<Uuid>,
    RequestJson(request): RequestJson<SendMessageRequest>,
) -> ApiResult<Response> {
    let input = SendInput {
        content: request.content,
        request_id: request.request_id,
        attachment_ids: request.attachment_ids.unwrap_or_default(),
        web_search: request.web_search.is_some_and(|options| options.enabled),
    };
    // The setup keeps the request span, so the turn records the trace id of the request.
    let setup =
        tokio::spawn(async move { stream.start(&ctx, chat_id, input).await }.in_current_span());
    let events = setup
        .await
        .map_err(|err| DomainError::internal(format!("the stream setup task failed: {err}")))??;
    Ok(sse_response(events))
}

/// `POST /v1/chats/{id}/turns/{request_id}/retry`: runs in a spawned task like the send handler,
/// so a client that disconnects between the mutation commit and the response still gets its new
/// turn finalized.
pub(crate) async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(mutations): Extension<Arc<TurnMutationService>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let setup = tokio::spawn(
        async move { mutations.retry(&ctx, chat_id, request_id).await }.in_current_span(),
    );
    let events = setup
        .await
        .map_err(|err| DomainError::internal(format!("the retry task failed: {err}")))??;
    Ok(sse_response(events))
}

/// `PATCH /v1/chats/{id}/turns/{request_id}`: edits the latest turn and streams the new answer.
pub(crate) async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(mutations): Extension<Arc<TurnMutationService>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
    RequestJson(request): RequestJson<EditTurnRequest>,
) -> ApiResult<Response> {
    let setup = tokio::spawn(
        async move {
            mutations
                .edit(&ctx, chat_id, request_id, request.content)
                .await
        }
        .in_current_span(),
    );
    let events = setup
        .await
        .map_err(|err| DomainError::internal(format!("the edit task failed: {err}")))??;
    Ok(sse_response(events))
}

/// `DELETE /v1/chats/{id}/turns/{request_id}`: soft-deletes the latest turn; 204.
pub(crate) async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(mutations): Extension<Arc<TurnMutationService>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    let task = tokio::spawn(
        async move { mutations.delete(&ctx, chat_id, request_id).await }.in_current_span(),
    );
    task.await
        .map_err(|err| DomainError::internal(format!("the delete task failed: {err}")))??;
    Ok(no_content())
}

/// `GET /v1/chats/{id}/messages`: `OData` filter, order and cursor pagination.
pub(crate) async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(messages): Extension<Arc<MessageService>>,
    Path(chat_id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<JsonBody<Page<MessageDto>>> {
    let page = messages.list(&ctx, chat_id, &query).await?;
    Ok(Json(page.map_items(MessageDto::from)))
}

/// `GET /v1/chats/{id}/turns/{request_id}`.
pub(crate) async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(turns): Extension<Arc<TurnService>>,
    Path((chat_id, request_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<JsonBody<TurnStatusDto>> {
    let status = turns.status(&ctx, chat_id, request_id).await?;
    Ok(Json(TurnStatusDto::from(status)))
}

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
pub(crate) async fn set_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(messages): Extension<Arc<MessageService>>,
    Path((chat_id, message_id)): Path<(Uuid, Uuid)>,
    RequestJson(request): RequestJson<SetReactionRequest>,
) -> ApiResult<JsonBody<ReactionDto>> {
    let reaction = messages
        .set_reaction(&ctx, chat_id, message_id, &request.reaction)
        .await?;
    Ok(Json(ReactionDto::from(reaction)))
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`: 204, idempotent.
pub(crate) async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(messages): Extension<Arc<MessageService>>,
    Path((chat_id, message_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    messages.delete_reaction(&ctx, chat_id, message_id).await?;
    Ok(no_content())
}

/// `GET /v1/quota/status`: the caller's remaining quota per tier and period.
pub(crate) async fn quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(quota): Extension<Arc<QuotaStatusService>>,
) -> ApiResult<JsonBody<QuotaStatusDto>> {
    let status = quota.status(&ctx).await?;
    Ok(Json(QuotaStatusDto::from(status)))
}

/// `POST /v1/chats/{id}/attachments`: the chat and its model are checked before the body is read;
/// the body is a multipart stream with a `file` part. 201 with the `AttachmentDetail`.
pub(crate) async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(attachments): Extension<Arc<AttachmentService>>,
    Path(chat_id): Path<Uuid>,
    uri: Uri,
    request: axum::extract::Request,
) -> ApiResult<impl IntoResponse> {
    let session = attachments.begin_upload(&ctx, chat_id).await?;
    let (parts, body) = request.into_parts();
    let (admission, file) = read_file(&attachments, &session, &parts.headers, body).await?;
    let detail = attachments.upload(session, admission, file).await?;
    let id = detail.id.to_string();
    Ok(created_json(AttachmentDetailDto::from(detail), &uri, &id))
}

/// `GET /v1/chats/{id}/attachments/{attachment_id}`.
pub(crate) async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(attachments): Extension<Arc<AttachmentService>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<JsonBody<AttachmentDetailDto>> {
    let detail = attachments.get(&ctx, chat_id, attachment_id).await?;
    Ok(Json(AttachmentDetailDto::from(detail)))
}

/// `DELETE /v1/chats/{id}/attachments/{attachment_id}`: 204, idempotent.
pub(crate) async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(attachments): Extension<Arc<AttachmentService>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    attachments.delete(&ctx, chat_id, attachment_id).await?;
    Ok(no_content())
}
