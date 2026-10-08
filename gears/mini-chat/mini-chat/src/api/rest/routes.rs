//! REST route registration.

use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto,
    MiniChatReactionDto, MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse,
    SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::handlers::{self, AppState};
use crate::domain::service::chats::ChatQueryFilterField;
use crate::domain::service::messages::MessageQueryFilterField;

const TAG: &str = "Mini Chat";

/// License requirement: the platform base feature.
struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

/// Register every mini-chat route.
#[allow(clippy::too_many_lines)]
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    state: AppState,
) -> Router {
    let p = state
        .service
        .cfg
        .url_prefix
        .trim_end_matches('/')
        .to_owned();
    let chats = format!("{p}/v1/chats");
    let chat = format!("{p}/v1/chats/{{id}}");

    router = OperationBuilder::post(&chats)
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<CreateChatReq>(openapi, "Chat title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "Path of the created chat",
            ResponseHeaderType::String,
        ))
        .standard_errors(openapi)
        .error_422(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(&chats)
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Opaque pagination cursor")
        .with_odata_filter::<ChatQueryFilterField>()
        .with_odata_orderby::<ChatQueryFilterField>()
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Chats")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(&chat)
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch(&chat)
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .json_request::<UpdateChatReq>(openapi, "New title")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(&chat)
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{chat}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Opaque pagination cursor")
        .with_odata_filter::<MessageQueryFilterField>()
        .with_odata_orderby::<MessageQueryFilterField>()
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Messages")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(format!("{chat}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(format!("{chat}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .multipart_file_request("file", Some("Attachment file"))
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::CREATED,
            "Uploaded attachment",
        )
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .register(router, openapi);

    let attachment = format!("{chat}/attachments/{{attachment_id}}");
    router = OperationBuilder::get(&attachment)
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(&attachment)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .register(router, openapi);

    let turn = format!("{chat}/turns/{{request_id}}");
    router = OperationBuilder::get(&turn)
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::post(format!("{turn}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the last turn")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .register(router, openapi);

    router = OperationBuilder::patch(&turn)
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the last turn")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(&turn)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the last turn")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .register(router, openapi);

    let reaction = format!("{chat}/messages/{{msg_id}}/reaction");
    router = OperationBuilder::put(&reaction)
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction")
        .standard_errors(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .register(router, openapi);

    router = OperationBuilder::delete(&reaction)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Models")
        .standard_errors(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Model id")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .standard_errors(openapi)
        .error_404(openapi)
        .register(router, openapi);

    router = OperationBuilder::get(format!("{p}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Quota status")
        .tag(TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .standard_errors(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}
