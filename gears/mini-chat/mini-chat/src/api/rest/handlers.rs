//! REST handlers of the Models API and the five chat endpoints.

use std::sync::Arc;

use axum::extract::Extension;
use axum::http::Uri;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use tokio_util::sync::CancellationToken;

use super::dto::{
    ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    ModelDto, ModelListDto, QuotaStatusResponseDto, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::stream;
use crate::domain::chats::{ChatsService, CreateChat};
use crate::domain::messages::MessagesService;
use crate::domain::mutations::{
    ReactionService, TurnMutationService, ValidatedEdit, parse_reaction, validate_edit_content,
};
use crate::domain::services::Services;
use crate::domain::streaming::{MessageStreamService, SendOutcome};

/// `GET /v1/models`
///
/// # Errors
///
/// A canonical Problem: 403 (deny or no usable decision), 503 (PDP failure), 500 (plugin failure).
pub async fn list_models(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
) -> ApiResult<Json<ModelListDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-request
    let models = services.models.list(&ctx).await?;
    Ok(Json(ModelListDto {
        items: models.into_iter().map(ModelDto::from).collect(),
    }))
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-request
}

/// `GET /v1/models/{id}`; `{id}` is an opaque string, not a UUID.
///
/// # Errors
///
/// A canonical Problem: 403, 404 (`not_found`, model resource type), 503, 500.
pub async fn get_model(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    extract::Path(id): extract::Path<String>,
) -> ApiResult<Json<ModelDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-request
    let model = services.models.get(&ctx, &id).await?;
    Ok(Json(ModelDto::from(model)))
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-request
}

/// The chat-management service, or a generic 500 when the gear was built without it.
fn chat_service(services: &Services) -> ApiResult<&Arc<ChatsService>> {
    services.chats.as_ref().ok_or_else(|| {
        tracing::error!("the chat service is not configured");
        CanonicalError::internal("the chat service is not configured").create()
    })
}

/// `POST /v1/chats`: 201 with the `ChatDetail` and `Location: {request path}/{id}`.
///
/// # Errors
///
/// A canonical Problem: 415, 400 or 422 for the body, 400 `INVALID_TITLE` or `INVALID_MODEL`,
/// 403 (deny or no usable decision), 503 (PDP failure), 500 (plugin or database failure).
pub async fn create_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    uri: Uri,
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-extract-body
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-no-content-type
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-415
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-bad-json
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-400-json
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-schema-mismatch
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-422
    // the platform JSON extractor answers 415 `missing_json_content_type`, 400
    // `json_syntax_error` and 422 `invalid_json_body`
    extract::Json(body): extract::Json<CreateChatReq>,
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-422
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-schema-mismatch
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-400-json
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-bad-json
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-415
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-no-content-type
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-extract-body
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-request
    let chats = chat_service(&services)?;
    let input = CreateChat {
        title: body.title,
        model: body.model,
    };
    let detail = chats.create(&ctx, input).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-request
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-201
    // `Location` is the request path as the gear sees it (without the api-gateway
    // `prefix_path`) plus the new id
    let id = detail.id.to_string();
    Ok(created_json(ChatDetailDto::from(detail), &uri, &id))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-201
}

/// `GET /v1/chats`: one page of the caller's chats.
///
/// # Errors
///
/// A canonical Problem: 400 for a rejected query (`OData` resource type), 403 (deny or no usable
/// decision), 503 (PDP failure), 500.
pub async fn list_chats(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-unparsable
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return-params
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-unsupported
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return-unsupported
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-select
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-zero
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-unparsable
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-duplicate
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-cursor-with-order
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return-order-with-cursor
    // the platform `OData` extractor binds `$filter`, `$orderby`, `$select`, `limit` (or `$top`)
    // and `cursor`: an unparsable query string is `INVALID_QUERY_PARAMS`, another `$` option
    // `UNSUPPORTED_QUERY_PARAM`, a bad `$select` `INVALID_SELECT` (a valid one is ignored), a
    // zero limit `INVALID_LIMIT`, an unparsable or duplicated limit `INVALID_QUERY_PARAMS` and a
    // cursor together with `$orderby` `ORDER_WITH_CURSOR`
    OData(query): OData,
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return-order-with-cursor
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-cursor-with-order
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-duplicate
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-unparsable
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-limit-zero
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-select
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return-unsupported
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-unsupported
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-return-params
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-list-query:p1:inst-query-unparsable
) -> ApiResult<JsonPage<ChatDetailDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-request
    let chats = chat_service(&services)?;
    let page = chats.list(&ctx, &query).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-request
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return
    Ok(Json(page.map_items(ChatDetailDto::from)))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-list-chats:p1:inst-list-return
}

/// `GET /v1/chats/{id}`.
///
/// # Errors
///
/// A canonical Problem: 400 `invalid_path_params`, 403, 404 (chat resource type), 503, 500.
pub async fn get_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-bad-path
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-return-400-path
    // a path id that is not a UUID is 400 `invalid_path_params`
    extract::Path(id): extract::Path<Uuid>,
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-return-400-path
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-bad-path
) -> ApiResult<Json<ChatDetailDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-request
    let chats = chat_service(&services)?;
    let detail = chats.get(&ctx, id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-request
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-return
    Ok(Json(ChatDetailDto::from(detail)))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-get-chat:p1:inst-get-return
}

/// `PATCH /v1/chats/{id}`: rename the chat; every body field other than `title` is ignored.
///
/// # Errors
///
/// A canonical Problem: 400 (`invalid_path_params`, `json_syntax_error`, `INVALID_TITLE`), 415,
/// 422, 403, 404 (chat resource type), 503, 500.
pub async fn update_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-bad-path
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-400-path
    extract::Path(id): extract::Path<Uuid>,
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-400-path
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-bad-path
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-extract-body
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-body-rejected
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-body-error
    // the platform JSON extractor answers 415 `missing_json_content_type`, 400
    // `json_syntax_error` and 422 `invalid_json_body` (a missing, `null` or non-string title)
    extract::Json(body): extract::Json<UpdateChatReq>,
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return-body-error
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-body-rejected
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-extract-body
) -> ApiResult<Json<ChatDetailDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-request
    let chats = chat_service(&services)?;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-request
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-ignore-fields
    // only `title` of the body reaches the service: `model`, `is_temporary` and every other
    // field are ignored
    let detail = chats.update_title(&ctx, id, &body.title).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-ignore-fields
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return
    Ok(Json(ChatDetailDto::from(detail)))
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-update-chat-title:p1:inst-update-return
}

/// `DELETE /v1/chats/{id}`: 204 without waiting for the provider cleanup.
///
/// # Errors
///
/// A canonical Problem: 400 `invalid_path_params` or an oversized cleanup payload, 403, 404
/// (chat resource type), 503, 500.
pub async fn delete_chat(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-bad-path
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-400-path
    extract::Path(id): extract::Path<Uuid>,
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-400-path
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-bad-path
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-request
    let chats = chat_service(&services)?;
    chats.delete(&ctx, id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-request
    // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-chat-management-delete-chat:p1:inst-delete-return-204
}

/// `GET /v1/quota/status`: the tiers and periods of the authenticated user.
///
/// # Errors
///
/// A canonical Problem: 403 (deny or no usable decision), 503 (PDP failure), 500 (plugin or
/// database failure).
pub async fn get_quota_status(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
) -> ApiResult<Json<QuotaStatusResponseDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-policy-failed
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return-500-policy
    // a plugin failure is a generic 500 and the cause is logged by the error mapping
    let quota = services.quota.as_ref().ok_or_else(|| {
        tracing::error!("the quota service is not configured");
        CanonicalError::internal("the quota service is not configured").create()
    })?;
    let status = quota.quota_status(&ctx).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return-500-policy
    // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-policy-failed
    Ok(Json(QuotaStatusResponseDto::from(status)))
}

/// The messages service, or a generic 500 when the gear was built without it.
fn messages_service(services: &Services) -> ApiResult<&Arc<MessagesService>> {
    services.messages.as_ref().ok_or_else(|| {
        tracing::error!("the messages service is not configured");
        CanonicalError::internal("the messages service is not configured").create()
    })
}

/// The send setup service, or a generic 500 when the gear was built without it.
fn streaming_service(services: &Services) -> ApiResult<&Arc<MessageStreamService>> {
    services.streaming.as_ref().ok_or_else(|| {
        tracing::error!("the streaming service is not configured");
        CanonicalError::internal("the streaming service is not configured").create()
    })
}

/// `GET /v1/chats/{id}/messages`: one page of the messages of the chat.
///
/// # Errors
///
/// A canonical Problem: 400 (`invalid_path_params`, or a rejected query of the `OData` resource
/// type), 403, 404 (chat resource type), 503, 500 (a NULL `request_id` or a database failure).
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    extract::Path(id): extract::Path<Uuid>,
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-validate
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-invalid
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-return-400
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-shared
    // the platform `OData` extractor binds `$filter`, `$orderby`, `$select`, `limit` (or `$top`)
    // and `cursor`: an unparsable query string is `INVALID_QUERY_PARAMS`, another `$` option
    // `UNSUPPORTED_QUERY_PARAM`, a bad `$select` `INVALID_SELECT` (a valid one is ignored), a
    // zero limit `INVALID_LIMIT`, an unparsable or duplicated limit `INVALID_QUERY_PARAMS`, an
    // over-long or over-complex filter `FILTER_TOO_LONG` and `FILTER_TOO_COMPLEX`; every one is
    // a 400 of the `OData` query resource type
    OData(query): OData,
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-message-list-query:p1:inst-mq-shared
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-return-400
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-validate
) -> ApiResult<JsonPage<MiniChatMessageDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-request
    let messages = messages_service(&services)?;
    let page = messages.list(&ctx, id, &query).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-list-messages:p1:inst-list-request
    Ok(Json(page.map_items(MiniChatMessageDto::from)))
}

/// `GET /v1/chats/{id}/turns/{request_id}`: the authoritative state of one turn.
///
/// # Errors
///
/// A canonical Problem: 400 `invalid_path_params`, 403, 404 (chat resource type for a chat that
/// is not visible, turn resource type for an unknown or soft-deleted turn), 503, 500.
pub async fn get_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<TurnStatusResponse>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-request
    let messages = messages_service(&services)?;
    let view = messages.turn_status(&ctx, id, request_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-turn-status:p1:inst-status-request
    Ok(Json(TurnStatusResponse::from(view)))
}

/// `POST /v1/chats/{id}/messages:stream`: validation, then the send setup; every failure before
/// the stream opens is a JSON Problem, a replay is served as `text/event-stream`.
///
/// # Errors
///
/// A canonical Problem: 415, 400 or 422 for the body, 400 for `EMPTY_CONTENT`,
/// `invalid_attachment`, `TOO_MANY_IMAGES`, `INVALID_MODEL`, `INPUT_TOO_LONG`,
/// `CONTEXT_BUDGET_EXCEEDED`, `VISION_NOT_SUPPORTED` and `FEATURE_DISABLED`, 403, 404, 409
/// (`request_id_conflict`, `turn_already_running`), 429, 503, 500.
pub async fn stream_message(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-path
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-path
    // a path id that is not a UUID is 400 `invalid_path_params`
    extract::Path(id): extract::Path<Uuid>,
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-path
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-path
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-content-type
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-415
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-json-syntax
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-syntax
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-schema
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-422
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-body
    // the platform JSON extractor answers 415 `missing_json_content_type`, 400
    // `json_syntax_error` and 422 `invalid_json_body` (a missing `content`, a wrong type, a
    // non-UUID `request_id` or `attachment_ids` entry, a `web_search` without a boolean
    // `enabled`); unknown body fields are ignored
    extract::Json(body): extract::Json<StreamMessageRequest>,
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-body
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-422
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-schema
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-syntax
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-json-syntax
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-return-415
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-request-validation:p1:inst-val-content-type
) -> ApiResult<axum::response::Response> {
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-send
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-request
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-request
    // a resend of a known `request_id` is the same request: the setup answers it with a replay
    // or a 409
    let streaming = Arc::clone(streaming_service(&services)?);
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-resend-request-id:p1:inst-resend-request
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-request
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-send
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-validate
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-invalid
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-invalid
    // the body checks run before the PEP call and before the chat is loaded, so they never
    // depend on chat existence; a rejection is a JSON Problem and no stream is opened
    let send = streaming.validate(body.into())?;
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-return-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-validate

    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-cancel-token
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-cancel-token
    // the turn's cancellation token exists before the setup starts; the guard cancels it when
    // the handler future is dropped (the connection went away before the stream opened) and
    // then travels into the response body, which cancels it when the body is dropped
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-cancel-token
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-cancel-token
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-spawn-setup
    // the remaining setup runs in a spawned task whose result is awaited; dropping the handler
    // future (a client disconnect before the stream opens) only detaches the task, which runs
    // on and, once the reserve committed, ends the turn itself (`cancelled` when the token is
    // cancelled before the provider call, `failed` when the provider task cannot start)
    let setup_token = cancel.clone();
    let ping_interval = streaming.settings.sse_ping_interval;
    let setup = tokio::spawn(async move { streaming.send(&ctx, id, send, &setup_token).await });
    let outcome = setup.await.map_err(|error| {
        tracing::error!(%error, "the send setup task failed");
        CanonicalError::internal("the send setup task failed").create()
    })??;
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-spawn-setup
    Ok(stream_response(outcome, guard, ping_interval))
}

/// The SSE response of a setup outcome (`messages:stream`, retry and edit share it).
fn stream_response(
    outcome: SendOutcome,
    guard: tokio_util::sync::DropGuard,
    ping_interval: std::time::Duration,
) -> axum::response::Response {
    match outcome {
        SendOutcome::Replay(events) => stream::replay_response(events, guard),
        SendOutcome::Streaming(started) => stream::relay_response(*started, guard, ping_interval),
        SendOutcome::CancelledBeforeProvider(reserved) => {
            // the setup task already finalized the turn as `cancelled`
            tracing::warn!(
                turn_id = %reserved.turn_id,
                "the connection was dropped after the reserve committed; no provider request was sent"
            );
            stream::silent_response(guard)
        }
    }
}

/// The turn mutation service, or a generic 500 when the gear was built without it.
fn mutation_service(services: &Services) -> ApiResult<&Arc<TurnMutationService>> {
    services.mutations.as_ref().ok_or_else(|| {
        tracing::error!("the turn mutation service is not configured");
        CanonicalError::internal("the turn mutation service is not configured").create()
    })
}

/// The reactions service, or a generic 500 when the gear was built without it.
fn reaction_service(services: &Services) -> ApiResult<&Arc<ReactionService>> {
    services.reactions.as_ref().ok_or_else(|| {
        tracing::error!("the reactions service is not configured");
        CanonicalError::internal("the reactions service is not configured").create()
    })
}

/// Run a retry or an edit in a spawned task whose result is awaited, as the send flow does, and
/// answer its stream.
async fn replacement_stream(
    mutations: Arc<TurnMutationService>,
    ctx: SecurityContext,
    chat_id: Uuid,
    request_id: Uuid,
    edit: Option<ValidatedEdit>,
) -> ApiResult<axum::response::Response> {
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-spawn-setup
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-spawn-setup
    // the turn's cancellation token exists before the setup starts; the guard cancels it when the
    // handler future is dropped and then travels into the response body
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();
    // the remaining setup runs in a spawned task whose result is awaited: a client disconnect
    // before the stream opens only detaches the task, which ends the replacement turn itself
    let setup_token = cancel.clone();
    let ping_interval = mutations.ping_interval();
    let setup = tokio::spawn(async move {
        match edit {
            Some(edit) => {
                mutations
                    .edit(&ctx, chat_id, request_id, edit, &setup_token)
                    .await
            }
            None => {
                mutations
                    .retry(&ctx, chat_id, request_id, &setup_token)
                    .await
            }
        }
    });
    let outcome = setup.await.map_err(|error| {
        tracing::error!(%error, "the mutation setup task failed");
        CanonicalError::internal("the mutation setup task failed").create()
    })??;
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-spawn-setup
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-spawn-setup
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-stream
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-stream
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-return
    // the stream follows the send contract unchanged: `stream_started` with the new `request_id`,
    // the relay and one terminal event; no separate settlement or outbox path exists
    Ok(stream_response(outcome, guard, ping_interval))
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-return
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-stream
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-stream
}

/// `POST /v1/chats/{id}/turns/{request_id}/retry`: retry the latest terminal turn and stream
/// the new answer.
///
/// # Errors
///
/// A canonical Problem before the stream opens.
pub async fn retry_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-bad-path
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-bad-path
    // a path id that is not a UUID is 400 `invalid_path_params`
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-return-bad-path
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-bad-path
) -> ApiResult<axum::response::Response> {
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-request
    // no body; admission runs inside the mutation service
    let mutations = Arc::clone(mutation_service(&services)?);
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn:p1:inst-retry-request
    replacement_stream(mutations, ctx, id, request_id, None).await
}

/// `PATCH /v1/chats/{id}/turns/{request_id}`: edit the latest terminal turn and stream the new
/// answer.
///
/// # Errors
///
/// A canonical Problem before the stream opens.
pub async fn edit_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-path
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-path
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-body
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-body
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-path
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-path
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-content-type
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-415
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-json-syntax
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-syntax
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-schema
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-422
    // the platform JSON extractor answers 415 `missing_json_content_type`, 400
    // `json_syntax_error` and 422 `invalid_json_body` (no `content`, or not a string)
    extract::Json(body): extract::Json<EditTurnRequest>,
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-422
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-schema
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-syntax
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-json-syntax
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-return-415
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-content-type
) -> ApiResult<axum::response::Response> {
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-validate
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-invalid
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-return-invalid
    // the body checks run before the PEP call and before the chat is loaded; a rejection is a
    // JSON Problem and no stream is opened
    let edit = validate_edit_content(body.content)?;
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-return-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-validate
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-request
    let mutations = Arc::clone(mutation_service(&services)?);
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn:p1:inst-edit-request
    replacement_stream(mutations, ctx, id, request_id, Some(edit)).await
}

/// `DELETE /v1/chats/{id}/turns/{request_id}`: soft-delete the latest terminal turn.
///
/// # Errors
///
/// A canonical Problem.
pub async fn delete_turn(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-bad-path
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-bad-path
    extract::Path((id, request_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-bad-path
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-bad-path
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-request
    let mutations = mutation_service(&services)?;
    mutations.delete(&ctx, id, request_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-request
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-delete-turn:p1:inst-del-return-204
}

/// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`: like or dislike an assistant message.
///
/// # Errors
///
/// A canonical Problem.
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-path
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-path
    extract::Path((id, message_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-path
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-path
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-content-type
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-415
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-json-syntax
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-syntax
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-schema
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-422
    // the platform JSON extractor answers 415, `json_syntax_error` and 422 (no `reaction`, or
    // not a string)
    extract::Json(body): extract::Json<SetReactionReq>,
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-422
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-schema
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-syntax
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-json-syntax
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-415
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-content-type
) -> ApiResult<Json<MiniChatReactionDto>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-request
    let reactions = reaction_service(&services)?;
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-request
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-validate
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-invalid
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-return-invalid
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return
    // the value is checked before authorization: 400 `INVALID_REACTION`
    let reaction = parse_reaction(&body.reaction)?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-return-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-invalid
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-validate
    let stored = reactions.set(&ctx, id, message_id, reaction).await?;
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-return-200
    Ok(Json(MiniChatReactionDto::from(stored)))
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction:p1:inst-setr-return-200
}

/// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`: remove the caller's reaction.
///
/// # Errors
///
/// A canonical Problem.
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-validate
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-delete
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-delete
    // a path id that is not a UUID is 400 `invalid_path_params`; the endpoint has no body
    extract::Path((id, message_id)): extract::Path<(Uuid, Uuid)>,
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-return-delete
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-request-validation:p1:inst-rv-delete
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-validate
) -> ApiResult<impl IntoResponse> {
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-request
    let reactions = reaction_service(&services)?;
    reactions.remove(&ctx, id, message_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-request
    // @cpt-begin:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-return-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction:p1:inst-delr-return-204
}

/// `POST /v1/chats/{id}/attachments`: the upload of one `file` part as `multipart/form-data`.
/// The body is handed to the service as a stream and is not read before admission.
///
/// # Errors
///
/// A canonical Problem of the attachment error contract: 400 `invalid_path_params` for a path id
/// that is not a UUID, the admission refusals, 404, 400, 429, 409, 500 and 503.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    uri: Uri,
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-path
    // a path id that is not a UUID is 400 `invalid_path_params`, before the body is read
    extract::Path(id): extract::Path<Uuid>,
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-path
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> ApiResult<impl IntoResponse> {
    use futures::StreamExt as _;
    let attachments = services.attachments.as_ref().ok_or_else(|| {
        CanonicalError::internal("the attachment service is not configured").create()
    })?;
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-request
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-request
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let stream = body
        .into_data_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other))
        .boxed();
    let record = attachments
        .upload(
            &ctx,
            id,
            crate::domain::attachments::UploadInput {
                content_type,
                body: stream,
            },
        )
        .await?;
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-image:p1:inst-img-request
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document:p1:inst-doc-request
    let attachment_id = record.id.to_string();
    Ok(created_json(
        super::dto::AttachmentDetailDto::from(record),
        &uri,
        &attachment_id,
    ))
}

/// `GET /v1/chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
///
/// A canonical Problem: 400 `invalid_path_params`, the admission refusals, 404 for the chat or
/// the attachment resource type, 500 and 503.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<super::dto::AttachmentDetailDto>> {
    let attachments = services.attachments.as_ref().ok_or_else(|| {
        CanonicalError::internal("the attachment service is not configured").create()
    })?;
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-request
    let record = attachments.get(&ctx, id, attachment_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-request
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-return-200
    Ok(Json(super::dto::AttachmentDetailDto::from(record)))
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment:p1:inst-get-return-200
}

/// `DELETE /v1/chats/{id}/attachments/{attachment_id}`: 204 without waiting for any provider
/// call, also for an already deleted attachment.
///
/// # Errors
///
/// A canonical Problem: 400 `invalid_path_params`, the admission refusals, 404 for the chat or
/// the attachment resource type, 409 `attachment_locked`, 500 and 503.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(services): Extension<Arc<Services>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    let attachments = services.attachments.as_ref().ok_or_else(|| {
        CanonicalError::internal("the attachment service is not configured").create()
    })?;
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-request
    // the admission, the delete transaction and the result match run in the service
    attachments.delete(&ctx, id, attachment_id).await?;
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-request
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-204
    Ok(no_content())
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment:p1:inst-del-case-204
}
