//! Route registration (operation ids, tags and responses per
//! `docs/api/api.json`).

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ResponseHeaderSpec, ResponseHeaderType, ThrottlingSpec,
};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto,
    MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse, ReactionDto, SetReactionReq,
    StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::gear::AppState;
use crate::infra::db::repos::chat_repo::ChatFilterField;
use crate::infra::db::repos::message_repo::MessageFilterField;

const TAG_MODELS: &str = "Mini Chat Models";
const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_QUOTAS: &str = "Mini Chat Quotas";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";

/// The base license feature every mini-chat route requires (ADR-0008).
pub(crate) struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Registers every mini-chat route on a local router carrying
/// `Extension(Arc<AppState>)`; the caller merges it into the host router.
pub fn register_routes(openapi: &dyn OpenApiRegistry, state: Arc<AppState>) -> Router {
    let prefix = state.cfg.url_prefix.trim_end_matches('/').to_owned();
    let router = Router::new();
    let router = register_model_routes(router, openapi, &prefix);
    let router = register_chat_routes(router, openapi, &prefix);
    let router = register_message_routes(router, openapi, &prefix);
    let router = register_quota_routes(router, openapi, &prefix);
    let router = register_turn_mutation_routes(router, openapi, &prefix);
    let router = register_attachment_routes(router, openapi, &prefix);
    router.layer(Extension(state))
}

/// `create_chat` throttling (zones defined in the api-gateway config; observe
/// only).
fn create_chat_throttling() -> ThrottlingSpec {
    ThrottlingSpec {
        rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
        in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
        require_security_context: false,
        dry_run: true,
    }
}

fn register_chat_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let chats = format!("{prefix}/v1/chats");
    let chat = format!("{prefix}/v1/chats/{{id}}");

    let router = OperationBuilder::get(chats.clone())
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .description("The caller's chats, most recently active first (cursor pagination).")
        .tag(TAG_CHATS)
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<ChatFilterField>()
        .with_odata_orderby::<ChatFilterField>()
        .authenticated()
        .require_license_features([License])
        .handler(handlers::chats::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of chats",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(chats)
        .operation_id("mini_chat.create_chat")
        .summary("Create chat")
        .description("Creates a chat; the model defaults to the catalog default.")
        .tag(TAG_CHATS)
        .with_throttling(create_chat_throttling())
        .json_request::<CreateChatReq>(openapi, "Optional title and model")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::chats::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
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

    let router = OperationBuilder::delete(chat.clone())
        .operation_id("mini_chat.delete_chat")
        .summary("Delete chat")
        .description("Soft-deletes the chat; provider resources are cleaned up asynchronously.")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
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

    let router = OperationBuilder::get(chat.clone())
        .operation_id("mini_chat.get_chat")
        .summary("Get chat")
        .description("Chat metadata and message count.")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::chats::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::patch(chat)
        .operation_id("mini_chat.update_chat")
        .summary("Update chat title")
        .description("Renames the chat; no other field changes.")
        .tag(TAG_CHATS)
        .path_param("id", "Chat id")
        .json_request::<UpdateChatReq>(openapi, "New title")
        .authenticated()
        .require_license_features([License])
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
        .register(router, openapi)
}

fn register_message_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let router = OperationBuilder::get(format!("{prefix}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .description("The chat's messages in chronological order (cursor pagination).")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat id")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<MessageFilterField>()
        .with_odata_orderby::<MessageFilterField>()
        .authenticated()
        .require_license_features([License])
        .handler(handlers::messages::list_messages)
        .json_response_with_schema::<Page<MessageDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of messages",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{prefix}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send message (streaming)")
        .description(
            "Sends a user message and streams the assistant response as SSE \
             (stream_started, ping, delta, tool, citations, done | error). A completed \
             request_id is replayed without a new provider call.",
        )
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat id")
        .json_request::<StreamMessageRequest>(openapi, "Message content and options")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::stream::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE of chat response events")
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

    let reaction = format!("{prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction");
    let router = OperationBuilder::delete(reaction.clone())
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove reaction")
        .description("Removes the caller's reaction from an assistant message.")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features([License])
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

    let router = OperationBuilder::put(reaction)
        .operation_id("mini_chat.put_reaction")
        .summary("Set reaction")
        .description("Sets (or replaces) the caller's like/dislike on an assistant message.")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .json_request::<SetReactionReq>(openapi, "Reaction value")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::reactions::put_reaction)
        .json_response_with_schema::<ReactionDto>(openapi, StatusCode::OK, "Stored reaction")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::get(format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get turn status")
        .description("Authoritative state of a turn, for reconnect and recovery.")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::turns::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_model_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let router = OperationBuilder::get(format!("{prefix}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .description("Models that are globally enabled in the policy catalog.")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::models::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Enabled models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::get(format!("{prefix}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model")
        .description("One enabled model; disabled or unknown models are 404.")
        .tag(TAG_MODELS)
        .path_param("id", "Model id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::models::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_quota_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    OperationBuilder::get(format!("{prefix}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status")
        .description(
            "Per-tier, per-period quota breakdown for the caller: limits, used \
             (spent + reserved) and remaining credits, warning flags and next reset.",
        )
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([License])
        .handler(handlers::quota::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_turn_mutation_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    prefix: &str,
) -> Router {
    let turn = format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}");
    let router = OperationBuilder::post(format!("{turn}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry last turn")
        .description(
            "Soft-deletes the chat's latest turn and re-submits its user message as a new \
             turn with a server-generated request_id; streams the new response as SSE \
             (same contract as messages:stream).",
        )
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Request id of the latest turn")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::turns::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE of chat response events")
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

    let router = OperationBuilder::patch(turn.clone())
        .operation_id("mini_chat.edit_turn")
        .summary("Edit last turn")
        .description(
            "Soft-deletes the chat's latest turn and submits a new turn with the updated \
             content (the original attachments are kept); streams the new response as SSE \
             (same contract as messages:stream).",
        )
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Request id of the latest turn")
        .json_request::<EditTurnRequest>(openapi, "New message content")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::turns::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE of chat response events")
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

    OperationBuilder::delete(turn)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete last turn")
        .description("Soft-deletes the chat's latest turn (no new turn is created).")
        .tag(TAG_TURNS)
        .path_param("id", "Chat id")
        .path_param("request_id", "Request id of the latest turn")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::turns::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_attachment_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    prefix: &str,
) -> Router {
    let attachments = format!("{prefix}/v1/chats/{{id}}/attachments");
    let attachment = format!("{attachments}/{{attachment_id}}");

    let router = OperationBuilder::post(attachments)
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload attachment")
        .description(
            "Uploads a document or image to the chat (multipart field `file`). Returns the \
             attachment `ready`, or `uploaded` while document indexing continues in the \
             background.",
        )
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat id")
        .multipart_file_request("file", Some("File to upload"))
        .authenticated()
        .require_license_features([License])
        .handler(handlers::attachments::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::CREATED,
            "Uploaded attachment",
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

    let router = OperationBuilder::delete(attachment.clone())
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete attachment")
        .description(
            "Soft-deletes an attachment that no message references; the provider file is \
             deleted asynchronously.",
        )
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([License])
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

    OperationBuilder::get(attachment)
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment")
        .description("Status and metadata of an attachment uploaded by the caller.")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([License])
        .handler(handlers::attachments::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}
