//! Route registration (`OperationBuilder`) for the mini-chat REST API.

use std::sync::Arc;

use axum::Router;
use http::StatusCode;
use toolkit::api::operation_builder::{LicenseFeature, OperationBuilderODataExt, ThrottlingSpec};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ResponseHeaderSpec, ResponseHeaderType};

use super::dto;
use super::handlers;
use crate::domain::services::AppServices;

/// The platform base license feature (interim gate, ADR-0008).
struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register all routes.
#[allow(clippy::too_many_lines, reason = "one builder chain per route")]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    svc: Arc<AppServices>,
) -> Router {
    let router = OperationBuilder::post("/mini-chat/v1/chats")
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag("Mini Chat Chats")
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<dto::CreateChatReq>(openapi, "Chat creation data")
        .handler(handlers::create_chat)
        .json_response_with_schema::<dto::ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "Path of the created chat",
            ResponseHeaderType::String,
        ))
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: false,
        })
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/chats")
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag("Mini Chat Chats")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_chats)
        .json_response_with_schema::<toolkit_odata::Page<dto::ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of chats",
        )
        .with_odata_filter::<dto::ChatListQueryFilterField>()
        .with_odata_orderby::<dto::ChatListQueryFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/chats/{id}")
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag("Mini Chat Chats")
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::get_chat)
        .json_response_with_schema::<dto::ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::patch("/mini-chat/v1/chats/{id}")
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag("Mini Chat Chats")
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<dto::UpdateChatReq>(openapi, "Chat update data")
        .handler(handlers::update_chat)
        .json_response_with_schema::<dto::ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::delete("/mini-chat/v1/chats/{id}")
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag("Mini Chat Chats")
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/chats/{id}/messages")
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag("Mini Chat Messages")
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_messages)
        .json_response_with_schema::<toolkit_odata::Page<dto::MiniChatMessageDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of messages",
        )
        .with_odata_filter::<dto::MessageListQueryFilterField>()
        .with_odata_orderby::<dto::MessageListQueryFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::post("/mini-chat/v1/chats/{id}/messages:stream")
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag("Mini Chat Messages")
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<dto::StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream_message)
        .sse_json::<dto::MiniChatSseEvent>(openapi, "SSE stream of chat response events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::post("/mini-chat/v1/chats/{id}/attachments")
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag("Mini Chat Attachments")
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features::<License>([])
        .multipart_file_request("file", Some("File to upload (expects field 'file' with file data)"))
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<dto::AttachmentDetailDto>(
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
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/chats/{id}/attachments/{attachment_id}")
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag("Mini Chat Attachments")
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::get_attachment)
        .json_response_with_schema::<dto::AttachmentDetailDto>(
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
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::delete("/mini-chat/v1/chats/{id}/attachments/{attachment_id}")
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag("Mini Chat Attachments")
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/chats/{id}/turns/{request_id}")
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag("Mini Chat Turns")
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::get_turn)
        .json_response_with_schema::<dto::TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::post("/mini-chat/v1/chats/{id}/turns/{request_id}/retry")
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag("Mini Chat Turns")
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::retry_turn)
        .sse_json::<dto::MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::patch("/mini-chat/v1/chats/{id}/turns/{request_id}")
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag("Mini Chat Turns")
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<dto::EditTurnRequest>(openapi, "Replacement user message")
        .handler(handlers::edit_turn)
        .sse_json::<dto::MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::delete("/mini-chat/v1/chats/{id}/turns/{request_id}")
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag("Mini Chat Turns")
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::put("/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction")
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag("Mini Chat Reactions")
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<dto::SetReactionReq>(openapi, "Reaction data")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<dto::MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::delete("/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction")
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag("Mini Chat Reactions")
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/models")
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag("Mini Chat Models")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::list_models)
        .json_response_with_schema::<dto::ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/models/{id}")
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag("Mini Chat Models")
        .path_param("id", "Model identifier")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::get_model)
        .json_response_with_schema::<dto::ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    let router = OperationBuilder::get("/mini-chat/v1/quota/status")
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag("Mini Chat Quotas")
        .authenticated()
        .require_license_features::<License>([])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<dto::QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(ResponseHeaderSpec::new(
            "Retry-After",
            "Seconds to wait before retrying",
            ResponseHeaderType::Integer,
        ))
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}
