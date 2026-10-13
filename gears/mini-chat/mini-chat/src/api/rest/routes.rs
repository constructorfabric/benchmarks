//! Route registration (paths and operation ids as in `docs/api/api.json`).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt, ParamSpec, ResponseHeaderSpec,
    ResponseHeaderType, ThrottlingSpec,
};
use toolkit::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto, MiniChatSseEvent,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::service::MiniChatService;
use crate::domain::service::chats::ChatField;
use crate::domain::service::messages::MessageField;

/// Platform base license feature (interim `ai_chat` gate, ADR-0008).
pub struct BaseLicense;

impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for BaseLicense {}

const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_QUOTAS: &str = "Mini Chat Quotas";

/// Register every mini-chat route under `prefix` (e.g. `/mini-chat`).
#[allow(clippy::too_many_lines)]
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<MiniChatService>, prefix: &str) -> Router {
    let p = prefix.trim_end_matches('/');
    let chats = format!("{p}/v1/chats");
    let chat = format!("{p}/v1/chats/{{id}}");

    let router = OperationBuilder::post(chats.clone())
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG_CHATS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .json_request::<CreateChatReq>(openapi, "Chat title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
        .response_header(ResponseHeaderSpec::new("Location", "Path of the created chat", ResponseHeaderType::String))
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(chats)
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG_CHATS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .param(ParamSpec::query("limit").param_type("integer").description("Maximum number of chats to return"))
        .query_param("cursor", false, "Pagination cursor")
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Paginated chats")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(chat.clone())
        .operation_id("mini_chat.get_chat")
        .summary("Get chat metadata and message count")
        .tag(TAG_CHATS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(chat.clone())
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG_CHATS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
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

    let router = OperationBuilder::delete(chat.clone())
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{chat}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages of a chat")
        .tag(TAG_MESSAGES)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .param(ParamSpec::query("limit").param_type("integer").description("Maximum number of messages to return"))
        .query_param("cursor", false, "Pagination cursor")
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Paginated messages")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{chat}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(TAG_MESSAGES)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of mini-chat events")
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

    let att = format!("{chat}/attachments");
    let router = OperationBuilder::post(att.clone())
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG_ATTACHMENTS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .multipart_file_request("file", Some("File to upload"))
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Attachment uploaded and processed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let one_att = format!("{att}/{{attachment_id}}");
    let router = OperationBuilder::get(one_att.clone())
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment status and metadata")
        .tag(TAG_ATTACHMENTS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(one_att)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG_ATTACHMENTS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let turn = format!("{chat}/turns/{{request_id}}");
    let router = OperationBuilder::get(turn.clone())
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .tag(TAG_TURNS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(turn.clone())
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TAG_TURNS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of mini-chat events")
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

    let router = OperationBuilder::delete(turn.clone())
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the latest turn")
        .tag(TAG_TURNS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{turn}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TAG_TURNS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of mini-chat events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let reaction = format!("{chat}/messages/{{msg_id}}/reaction");
    let router = OperationBuilder::put(reaction.clone())
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction on an assistant message")
        .tag(TAG_REACTIONS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
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

    let router = OperationBuilder::delete(reaction)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG_REACTIONS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models visible to the user")
        .tag(TAG_MODELS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get one model")
        .tag(TAG_MODELS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Model identifier")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(TAG_QUOTAS)
        .exposed()
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}
