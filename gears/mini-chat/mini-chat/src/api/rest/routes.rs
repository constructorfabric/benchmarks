//! Route registration with OpenAPI metadata.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt, ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ThrottlingSpec};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto, MiniChatSseEvent,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::app::App;
use crate::domain::chats::{ChatField, MessageField};

/// Platform base license feature (interim gate, ADR-0008).
struct License;

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

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new("Retry-After", "Seconds to wait before retrying", ResponseHeaderType::Integer)
}

/// Registers every mini-chat route under `prefix` (default `/mini-chat`).
#[allow(clippy::too_many_lines)]
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, app: Arc<App>, prefix: &str) -> Router {
    let p = prefix.trim_end_matches('/');

    let router = OperationBuilder::post(format!("{p}/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .json_request::<CreateChatReq>(openapi, "Optional title and model")
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
        .response_header(ResponseHeaderSpec::new("Location", "URL of the created chat", ResponseHeaderType::String))
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .query_param_typed("limit", false, "Page size (default 20, max 100; larger values are clamped)", "integer")
        .query_param("cursor", false, "Opaque cursor from page_info")
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Chats")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Rename a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
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
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .query_param_typed("limit", false, "Page size (default 20, max 100; larger values are clamped)", "integer")
        .query_param("cursor", false, "Opaque cursor from page_info")
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Messages")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the answer")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG_ATTACHMENTS)
        .multipart_file_request("file", Some("File to upload"))
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the latest turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::put(format!("{p}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
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
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{p}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/models"))
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
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Model id")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Quota status of the calling user")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    router.layer(axum::Extension(app))
}
