//! Route registration with `OpenAPI` metadata.

use std::sync::Arc;

use axum::Router;
use http::StatusCode;
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
use super::handlers;
use super::odata::{ChatQueryFieldsFilterField, MessageQueryFieldsFilterField};
use crate::service::AppState;

const TAG: &str = "Mini Chat";

/// Platform base license feature (every route requires it).
pub struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

#[allow(clippy::too_many_lines)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<AppState>,
) -> Router {
    let p = state.cfg.url_prefix.trim_end_matches('/').to_owned();
    let path = |s: &str| format!("{p}/v1{s}");

    router = OperationBuilder::post(path("/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([License])
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "URL of the created chat",
            ResponseHeaderType::String,
        ))
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG)
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Opaque pagination cursor")
        .with_odata_filter::<ChatQueryFieldsFilterField>()
        .with_odata_orderby::<ChatQueryFieldsFilterField>()
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_chats)
        .json_response_with_schema::<toolkit_odata::Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Chats",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/chats/{id}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get chat")
        .tag(TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch(path("/chats/{id}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update chat title")
        .tag(TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(path("/chats/{id}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete chat")
        .tag(TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/chats/{id}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .tag(TAG)
        .path_param("id", "Chat id")
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Opaque pagination cursor")
        .with_odata_filter::<MessageQueryFieldsFilterField>()
        .with_odata_orderby::<MessageQueryFieldsFilterField>()
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_messages)
        .json_response_with_schema::<toolkit_odata::Page<MessageDto>>(
            openapi,
            StatusCode::OK,
            "Messages",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(path("/chats/{id}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response")
        .tag(TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the assistant response")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(path("/chats/{id}/turns/{request_id}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest turn")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the new assistant response")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch(path("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .json_request::<EditTurnRequest>(openapi, "Replacement user message")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the new assistant response")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(path("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the latest turn")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::put(path("/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([License])
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<ReactionDto>(openapi, StatusCode::OK, "Reaction")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(path("/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(path("/chats/{id}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .multipart_file_request(
            "file",
            Some("File to upload (expects field 'file' with file data)"),
        )
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::CREATED,
            "Uploaded attachment",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(path("/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .tag(TAG)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Models")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/models/{id}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .tag(TAG)
        .path_param("id", "Model id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(path("/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Quota status")
        .tag(TAG)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .standard_errors(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(state))
}
