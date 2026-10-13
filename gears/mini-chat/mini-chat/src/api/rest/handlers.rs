//! Handlers of the Models API and of the chat CRUD feature.
//!
//! Path, query and body are parsed by the platform extractors, whose failures
//! are the platform `invalid_argument` problems.

use std::sync::Arc;

use axum::extract::Extension;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_security::SecurityContext;

use axum::http::Uri;
use toolkit_odata::Page;
use uuid::Uuid;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto,
    MiniChatReactionDto, ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq,
    StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::multipart::RawMultipart;
use super::odata_guard::GuardedOData;
use super::sse::sse_response;
use crate::domain::service::attachments::service::AttachmentsService;
use crate::domain::service::chats::ChatsService;
use crate::domain::service::messages::MessagesService;
use crate::domain::service::models::ModelsService;
use crate::domain::service::mutations::service::MutationService;
use crate::domain::service::quota::service::QuotaService;
use crate::domain::service::stream::pipeline::{SendOutcome, StreamService};
use crate::domain::service::stream::validate::SendRequest;

/// `GET /v1/models`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ModelsService>>,
) -> ApiResult<Json<ModelListDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-call-domain
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-domain-error
    let views = service.list_models(&ctx).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-domain-error
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-call-domain
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-return-list
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-success
    Ok(Json(ModelListDto {
        items: views.into_iter().map(ModelDto::from).collect(),
    }))
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-success
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-return-list
}

/// `GET /v1/models/{id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ModelsService>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-extract
    extract::Path(id): extract::Path<String>,
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-extract
) -> ApiResult<Json<ModelDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-call-domain
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-domain-error
    let view = service.get_model(&ctx, &id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-domain-error
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-call-domain
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-return-model
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-success
    Ok(Json(ModelDto::from(view)))
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-success
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-return-model
}

/// `POST /v1/chats`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ChatsService>>,
    uri: Uri,
    extract::Json(req): extract::Json<CreateChatReq>,
) -> ApiResult<impl IntoResponse> {
    let detail = service
        .create_chat(&ctx, req.title.as_deref(), req.model.as_deref())
        .await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-return-201
    let id = detail.id.to_string();
    Ok(created_json(ChatDetailDto::from(detail), &uri, &id))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-return-201
}

/// `GET /v1/chats`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ChatsService>>,
    GuardedOData(query): GuardedOData,
) -> ApiResult<JsonPage<ChatDetailDto>> {
    let page = service.list_chats(&ctx, &query).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-return-chat-page
    Ok(Json(Page {
        items: page.items.into_iter().map(ChatDetailDto::from).collect(),
        page_info: page.page_info,
    }))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-return-chat-page
}

/// `GET /v1/chats/{id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ChatsService>>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<Json<ChatDetailDto>> {
    let detail = service.get_chat(&ctx, id).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-return-chat
    Ok(Json(ChatDetailDto::from(detail)))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-return-chat
}

/// `PATCH /v1/chats/{id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn rename_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ChatsService>>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<UpdateChatReq>,
) -> ApiResult<Json<ChatDetailDto>> {
    let detail = service.rename_chat(&ctx, id, &req.title).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-return-renamed
    Ok(Json(ChatDetailDto::from(detail)))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-return-renamed
}

/// `DELETE /v1/chats/{id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ChatsService>>,
    extract::Path(id): extract::Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    service.delete_chat(&ctx, id).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-return-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-return-204
}

/// `GET /v1/chats/{id}/messages`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<MessagesService>>,
    extract::Path(id): extract::Path<Uuid>,
    GuardedOData(query): GuardedOData,
) -> ApiResult<JsonPage<MiniChatMessageDto>> {
    let page = service.list_messages(&ctx, id, &query).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-return-message-page
    Ok(Json(Page {
        items: page
            .items
            .into_iter()
            .map(MiniChatMessageDto::from)
            .collect(),
        page_info: page.page_info,
    }))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-return-message-page
}

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn set_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<MessagesService>>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<SetReactionReq>,
) -> ApiResult<Json<MiniChatReactionDto>> {
    let stored = service
        .set_reaction(&ctx, id, msg_id, &req.reaction)
        .await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-return-reaction
    Ok(Json(MiniChatReactionDto::from(stored)))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-return-reaction
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<MessagesService>>,
    extract::Path((id, msg_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    service.delete_reaction(&ctx, id, msg_id).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-return-reaction-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-return-reaction-204
}

/// `GET /v1/quota/status`
///
/// No query parameter is defined: any query string is ignored.
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<QuotaService>>,
) -> ApiResult<Json<QuotaStatusResponse>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-call-status
    let view = service.quota_status(&ctx).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-call-status
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-return-status
    Ok(Json(QuotaStatusResponse::from(view)))
    // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-return-status
}

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-send-endpoint:p1
/// `POST /v1/chats/{id}/messages:stream`
///
/// Every refusal before the stream opens is a canonical problem; after that
/// the response is `text/event-stream` and failures are SSE `error` events.
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-call-send
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<StreamService>>,
    extract::Path(id): extract::Path<Uuid>,
    extract::Json(req): extract::Json<StreamMessageRequest>,
) -> ApiResult<axum::response::Response> {
    let request = SendRequest {
        content: req.content,
        request_id: req.request_id,
        attachment_ids: req.attachment_ids.unwrap_or_default(),
        web_search_enabled: req.web_search.map(|web_search| web_search.enabled),
    };
    match service.send(ctx, id, request).await? {
        SendOutcome::Replay(events) => Ok(sse_response(futures::stream::iter(events))),
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-open-stream
        // @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-stream
        SendOutcome::Stream { started, relay } => Ok(sse_response(relay.into_stream(started))),
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-return-stream
        // @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-open-stream
    }
}
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-send-message:p1:inst-call-send

/// `GET /v1/chats/{id}/turns/{request_id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
// @cpt-begin:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-call-status
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<StreamService>>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    let status = service.turn_status(&ctx, id, request_id).await?;
    Ok(Json(TurnStatusResponse::from(status)))
}
// @cpt-end:cpt-cf-mini-chat-flow-streaming-turns-get-turn-status:p1:inst-call-status

/// `POST /v1/chats/{id}/turns/{request_id}/retry`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<MutationService>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-call-retry
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-call-retry
) -> ApiResult<axum::response::Response> {
    match service.retry(ctx, id, request_id).await? {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-open-retry-stream
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-return-retry-stream
        SendOutcome::Stream { started, relay } => Ok(sse_response(relay.into_stream(started))),
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-return-retry-stream
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-retry-turn:p1:inst-open-retry-stream
        SendOutcome::Replay(events) => Ok(sse_response(futures::stream::iter(events))),
    }
}

/// `PATCH /v1/chats/{id}/turns/{request_id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<MutationService>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-call-edit
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    extract::Json(req): extract::Json<EditTurnRequest>,
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-call-edit
) -> ApiResult<axum::response::Response> {
    match service.edit(ctx, id, request_id, req.content).await? {
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-open-edit-stream
        // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-return-edit-stream
        SendOutcome::Stream { started, relay } => Ok(sse_response(relay.into_stream(started))),
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-return-edit-stream
        // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-edit-turn:p1:inst-open-edit-stream
        SendOutcome::Replay(events) => Ok(sse_response(futures::stream::iter(events))),
    }
}

/// `DELETE /v1/chats/{id}/turns/{request_id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<MutationService>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-call-delete
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-call-delete
) -> ApiResult<impl IntoResponse> {
    service.delete(&ctx, id, request_id).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-return-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-delete-turn:p1:inst-return-204
}

/// `POST /v1/chats/{id}/attachments`
///
/// The handler reads the raw body as a stream (no body limit of the route); the
/// domain reads it only after the checks that precede the body.
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<AttachmentsService>>,
    extract::Path(id): extract::Path<Uuid>,
    request: axum::extract::Request,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-call-upload
    let content_type = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut source = RawMultipart::new(content_type.as_deref(), request.into_body());
    let detail = service.upload(&ctx, id, &mut source).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-call-upload
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-return-created
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(detail))))
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-upload-attachment:p1:inst-return-created
}

/// `GET /v1/chats/{id}/attachments/{attachment_id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<AttachmentsService>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-call-get
    let detail = service.get(&ctx, id, attachment_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-get-attachment:p1:inst-call-get
    Ok(Json(AttachmentDetailDto::from(detail)))
}

/// `DELETE /v1/chats/{id}/attachments/{attachment_id}`
///
/// # Errors
///
/// A canonical problem per the domain error mapping.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<AttachmentsService>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-call-delete
    service.delete(&ctx, id, attachment_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-delete-attachment:p1:inst-call-delete
    Ok(no_content())
}
