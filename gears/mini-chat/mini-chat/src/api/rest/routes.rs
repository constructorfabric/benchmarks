//! Route and `OpenAPI` registration.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ResponseHeaderSpec, ResponseHeaderType,
};

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto,
    MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse, ReactionDto, SetReactionReq,
    StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::{handlers, stream, upload};
use crate::domain::app::AppServices;
use crate::infra::db::odata::{ChatFilterField, MessageFilterField};

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
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_QUOTAS: &str = "Mini Chat Quotas";

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Registers every mini-chat route.
#[allow(clippy::too_many_lines)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    svc: Arc<AppServices>,
) -> Router {
    let p = format!("{}/v1", svc.cfg.url_prefix.trim_end_matches('/'));

    let router = OperationBuilder::get(format!("{p}/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_chats)
        .json_response_with_schema::<toolkit_odata::Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of chats",
        )
        .with_odata_filter::<ChatFilterField>()
        .with_odata_orderby::<ChatFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "URI of the created chat",
            ResponseHeaderType::String,
        ))
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(format!("{p}/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
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

    let router = OperationBuilder::delete(format!("{p}/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
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

    let router = OperationBuilder::get(format!("{p}/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_messages)
        .json_response_with_schema::<toolkit_odata::Page<MessageDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of messages",
        )
        .with_odata_filter::<MessageFilterField>()
        .with_odata_orderby::<MessageFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(stream::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat response events")
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

    let router = OperationBuilder::put(format!("{p}/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<ReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{p}/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([License])
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
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .multipart_file_request(
            "file",
            Some("File to upload (expects field 'file' with file data)"),
        )
        .handler(upload::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::CREATED,
            "Attachment uploaded and processed",
        )
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

    let router = OperationBuilder::get(format!("{p}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::OK,
            "Attachment metadata",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router =
        OperationBuilder::delete(format!("{p}/chats/{{id}}/attachments/{{attachment_id}}"))
            .operation_id("mini_chat.delete_attachment")
            .summary("Delete an attachment")
            .tag(TAG_ATTACHMENTS)
            .authenticated()
            .require_license_features([License])
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
            .response_header(retry_after())
            .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(format!("{p}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .json_request::<EditTurnRequest>(openapi, "Replacement user message")
        .handler(stream::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
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

    let router = OperationBuilder::delete(format!("{p}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
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
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(stream::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
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

    let router = OperationBuilder::get(format!("{p}/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License])
        .path_param("id", "Model identifier")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}
