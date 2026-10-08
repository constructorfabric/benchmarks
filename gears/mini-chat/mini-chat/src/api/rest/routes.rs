//! REST route registration (operation ids `mini_chat.<op>`).

use std::sync::Arc;

use axum::http::StatusCode;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ThrottlingSpec};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::odata_fields::{ChatField, MessageField};
use crate::domain::service::MiniChat;

const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_QUOTAS: &str = "Mini Chat Quotas";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register every route of the gear.
#[allow(clippy::too_many_lines, reason = "flat route table")]
pub fn register_routes(mut router: axum::Router, openapi: &dyn OpenApiRegistry, svc: Arc<MiniChat>) -> axum::Router {
    let p = svc.cfg.url_prefix.trim_end_matches('/').to_owned();

    router = OperationBuilder::post(format!("{p}/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .json_request::<CreateChatReq>(openapi, "Optional title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG_CHATS)
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Pagination cursor")
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Chats page")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .json_request::<UpdateChatReq>(openapi, "New title")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages of a chat")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat id")
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Pagination cursor")
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Messages page")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the turn")
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

    router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .multipart_file_request("file", Some("The file to upload"))
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Uploaded attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the last turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the new turn")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the last turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the new turn")
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

    router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the last turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(format!("{p}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([License])
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .tag(TAG_MODELS)
        .path_param("id", "Model id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Quota status of the caller")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}
