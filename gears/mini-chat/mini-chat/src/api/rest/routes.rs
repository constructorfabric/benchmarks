//! Route registration (`docs/api/api.json`, operation ids `mini_chat.*`).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, ThrottlingSpec};
use toolkit::api::{OpenApiRegistry, OperationBuilder};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto, MiniChatReactionDto,
    MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::service::AppServices;

const TAG: &str = "Mini Chat";

struct BaseLicense;

impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for BaseLicense {}

const FILTER_CHATS: &str = "OData v4 filter expression\n- updated_at: eq|ne|gt|ge|lt|le|in\n- id: eq|ne|in\n- title: eq|ne|contains|startswith|endswith|in";
const ORDER_CHATS: &str = "OData v4 orderby expression\n- updated_at asc\n- updated_at desc\n- id asc\n- id desc\n- title asc\n- title desc";
const FILTER_MESSAGES: &str = "OData v4 filter expression\n- created_at: eq|ne|gt|ge|lt|le|in\n- id: eq|ne|in\n- role: eq|ne|contains|startswith|endswith|in";
const ORDER_MESSAGES: &str = "OData v4 orderby expression\n- created_at asc\n- created_at desc\n- id asc\n- id desc\n- role asc\n- role desc";

/// Registers every mini-chat route.
#[allow(clippy::too_many_lines)]
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<AppServices>, prefix: &str) -> Router {
    let p = |s: &str| format!("{prefix}{s}");

    let router = OperationBuilder::post(p("/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .json_request::<CreateChatReq>(openapi, "Chat title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .query_param_typed("limit", false, "Maximum number of chats to return", "integer")
        .query_param("cursor", false, "Cursor for pagination")
        .query_param("$filter", false, FILTER_CHATS)
        .query_param("$orderby", false, ORDER_CHATS)
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Paginated list of chats")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/v1/chats/{id}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(p("/v1/chats/{id}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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

    let router = OperationBuilder::delete(p("/v1/chats/{id}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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

    let router = OperationBuilder::get(p("/v1/chats/{id}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .query_param_typed("limit", false, "Maximum number of messages to return", "integer")
        .query_param("cursor", false, "Cursor for pagination")
        .query_param("$filter", false, FILTER_MESSAGES)
        .query_param("$orderby", false, ORDER_MESSAGES)
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Paginated list of messages")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(p("/v1/chats/{id}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
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
        .register(router, openapi);

    let router = OperationBuilder::get(p("/v1/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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
        .register(router, openapi);

    let router = OperationBuilder::patch(p("/v1/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
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
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/v1/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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

    let router = OperationBuilder::post(p("/v1/chats/{id}/turns/{request_id}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(p("/v1/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/v1/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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

    let router = OperationBuilder::post(p("/v1/chats/{id}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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

    let router = OperationBuilder::get(p("/v1/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment metadata")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/v1/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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

    let router = OperationBuilder::get(p("/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/v1/models/{id}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
        .path_param("id", "Model identifier")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(TAG)
        .authenticated()
        .require_license_features([&BaseLicense])
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
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}
