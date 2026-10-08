//! REST route registration (`/v1/...` under `url_prefix`).

pub mod attachments;
pub mod dto;
pub mod handlers;
pub mod sse;

use std::sync::Arc;

use axum::Router;
use http::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt, ResponseHeaderSpec,
    ResponseHeaderType,
};
use toolkit::api::{OpenApiRegistry, OperationBuilder};
use toolkit_odata::Page;

use crate::domain::service::Svc;
use crate::infra::db::odata::{ChatField, MessageField};
use dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto,
    MiniChatReactionDto, MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse,
    SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};

/// Base license feature (ADR-0008 interim gate).
pub struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_QUOTAS: &str = "Mini Chat Quotas";

/// Registers every route.
#[allow(clippy::too_many_lines)]
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<Svc>) -> Router {
    let p = svc.cfg.url_prefix.trim_end_matches('/').to_owned();
    let path = |s: &str| format!("{p}/v1{s}");

    let router = OperationBuilder::post(path("/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
        .json_request::<CreateChatReq>(openapi, "Optional title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
        .response_header(ResponseHeaderSpec::new("Location", "URL of the created chat", ResponseHeaderType::String))
        .error_400(openapi).error_401(openapi).error_403(openapi).error_422(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Page of chats")
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .error_400(openapi).error_401(openapi).error_403(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/chats/{id}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(path("/chats/{id}"))
        .operation_id("mini_chat.update_chat")
        .summary("Rename a chat")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([&License])
        .json_request::<UpdateChatReq>(openapi, "New title")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_422(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(path("/chats/{id}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/chats/{id}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Page of messages")
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(path("/chats/{id}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the answer")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([&License])
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_409(openapi).error_422(openapi).error_429(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = attachments::register(router, openapi, &path);

    let router = OperationBuilder::get(path("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.get_turn")
        .summary("Turn status")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(path("/chats/{id}/turns/{request_id}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the last turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_409(openapi).error_429(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(path("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the last turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([&License])
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_409(openapi).error_422(openapi).error_429(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(path("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the last turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_409(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(path("/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([&License])
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_422(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(path("/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Models")
        .error_401(openapi).error_403(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/models/{id}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .tag(TAG_MODELS)
        .path_param("id", "Model id")
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Quota status")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .error_401(openapi).error_403(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let _ = (TAG_ATTACHMENTS, std::marker::PhantomData::<AttachmentDetailDto>);
    router.layer(axum::Extension(svc))
}
