//! Route registration (paths under the configured `url_prefix`).

use std::sync::Arc;

use axum::Router;
use http::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt,
};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ResponseHeaderSpec, ResponseHeaderType};

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, ChatDetailDtoFilterField, CreateChatReq, EditTurnRequest,
    MiniChatMessageDto, MiniChatMessageDtoFilterField, MiniChatReactionDto, MiniChatSseEvent,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::service::Services;

/// License features required by the routes (interim: platform base license, ADR-0008).
enum License {
    Base,
}

impl AsRef<str> for License {
    fn as_ref(&self) -> &str {
        match self {
            Self::Base => CORE_GLOBAL_BASE_LICENSE_FEATURE,
        }
    }
}

impl LicenseFeature for License {}

const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_QUOTAS: &str = "Mini Chat Quotas";

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Registers all mini-chat routes.
#[allow(clippy::too_many_lines)]
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    services: Arc<Services>,
    prefix: &str,
) -> Router {
    let p = prefix.trim_end_matches('/');

    // ── Chats ──────────────────────────────────────────────────────────────
    let router = OperationBuilder::get(format!("{p}/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats of the current user")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .query_param_typed("limit", false, "Maximum number of chats to return", "integer")
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<ChatDetailDtoFilterField>()
        .with_odata_orderby::<ChatDetailDtoFilterField>()
        .handler(handlers::chats::list_chats)
        .json_response_with_schema::<toolkit_odata::Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Page of chats",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{p}/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(handlers::chats::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "Path of the created chat",
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

    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .handler(handlers::chats::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat details")
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
        .summary("Update a chat title")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(handlers::chats::update_chat)
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
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .handler(handlers::chats::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    // ── Messages ───────────────────────────────────────────────────────────
    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages of a chat")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .query_param_typed("limit", false, "Maximum number of messages to return", "integer")
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<MiniChatMessageDtoFilterField>()
        .with_odata_orderby::<MiniChatMessageDtoFilterField>()
        .handler(handlers::messages::list_messages)
        .json_response_with_schema::<toolkit_odata::Page<MiniChatMessageDto>>(
            openapi,
            StatusCode::OK,
            "Page of messages",
        )
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
        .summary("Send a message and stream the response (SSE)")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream::stream_message)
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

    // ── Turns ──────────────────────────────────────────────────────────────
    let router = OperationBuilder::get(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request id")
        .handler(handlers::turns::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(format!("{p}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the last turn and regenerate (SSE)")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request id")
        .json_request::<EditTurnRequest>(openapi, "New message content")
        .handler(handlers::stream::edit_turn)
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
        .summary("Delete the last turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request id")
        .handler(handlers::stream::delete_turn)
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

    let router = OperationBuilder::post(format!(
        "{p}/v1/chats/{{id}}/turns/{{request_id}}/retry"
    ))
    .operation_id("mini_chat.retry_turn")
    .summary("Retry the last turn (SSE)")
    .tag(TAG_TURNS)
    .authenticated()
    .require_license_features::<License>([License::Base])
    .path_param("id", "Chat UUID")
    .path_param("request_id", "Turn request id")
    .handler(handlers::stream::retry_turn)
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

    // ── Attachments ────────────────────────────────────────────────────────
    let router = OperationBuilder::post(format!("{p}/v1/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .path_param("id", "Chat UUID")
        .multipart_file_request("file", Some("File to upload"))
        .handler(handlers::attachments::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::CREATED,
            "Attachment uploaded",
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

    let router = OperationBuilder::get(format!(
        "{p}/v1/chats/{{id}}/attachments/{{attachment_id}}"
    ))
    .operation_id("mini_chat.get_attachment")
    .summary("Get attachment status and metadata")
    .tag(TAG_ATTACHMENTS)
    .authenticated()
    .require_license_features::<License>([License::Base])
    .path_param("id", "Chat UUID")
    .path_param("attachment_id", "Attachment UUID")
    .handler(handlers::attachments::get_attachment)
    .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_404(openapi)
    .error_500(openapi)
    .error_503(openapi)
    .response_header(retry_after())
    .register(router, openapi);

    let router = OperationBuilder::delete(format!(
        "{p}/v1/chats/{{id}}/attachments/{{attachment_id}}"
    ))
    .operation_id("mini_chat.delete_attachment")
    .summary("Delete an attachment")
    .tag(TAG_ATTACHMENTS)
    .authenticated()
    .require_license_features::<License>([License::Base])
    .path_param("id", "Chat UUID")
    .path_param("attachment_id", "Attachment UUID")
    .handler(handlers::attachments::delete_attachment)
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

    // ── Reactions ──────────────────────────────────────────────────────────
    let router = OperationBuilder::put(format!(
        "{p}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"
    ))
    .operation_id("mini_chat.put_reaction")
    .summary("Set a reaction on an assistant message")
    .tag(TAG_REACTIONS)
    .authenticated()
    .require_license_features::<License>([License::Base])
    .path_param("id", "Chat UUID")
    .path_param("msg_id", "Message UUID")
    .json_request::<SetReactionReq>(openapi, "Reaction")
    .handler(handlers::reactions::put_reaction)
    .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_404(openapi)
    .error_422(openapi)
    .error_500(openapi)
    .error_503(openapi)
    .response_header(retry_after())
    .register(router, openapi);

    let router = OperationBuilder::delete(format!(
        "{p}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"
    ))
    .operation_id("mini_chat.delete_reaction")
    .summary("Remove a reaction from an assistant message")
    .tag(TAG_REACTIONS)
    .authenticated()
    .require_license_features::<License>([License::Base])
    .path_param("id", "Chat UUID")
    .path_param("msg_id", "Message UUID")
    .handler(handlers::reactions::delete_reaction)
    .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_404(openapi)
    .error_500(openapi)
    .error_503(openapi)
    .response_header(retry_after())
    .register(router, openapi);

    // ── Models ─────────────────────────────────────────────────────────────
    let router = OperationBuilder::get(format!("{p}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models visible to the current user")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .handler(handlers::models::list_models)
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
        .require_license_features::<License>([License::Base])
        .path_param("id", "Model identifier")
        .handler(handlers::models::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    // ── Quota ──────────────────────────────────────────────────────────────
    let router = OperationBuilder::get(format!("{p}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Per-tier, per-period quota status of the current user")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features::<License>([License::Base])
        .handler(handlers::quota::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    router.layer(axum::Extension(services))
}
