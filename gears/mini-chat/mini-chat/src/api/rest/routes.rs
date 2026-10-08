//! REST route registration (`{url_prefix}/v1/...`). Every route is
//! authenticated and gated by the platform base license feature.

use std::sync::Arc;

use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, ChatDetailDtoFilterField, CreateChatReq, EditTurnRequest,
    MiniChatMessageDto, MiniChatMessageDtoFilterField, MiniChatReactionDto, MiniChatSseEvent,
    ModelDto, ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest,
    TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::services::AppServices;

const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_QUOTAS: &str = "Mini Chat Quotas";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";
const TAG_REACTIONS: &str = "Mini Chat Reactions";

/// Platform base license feature (ADR-0008).
struct BaseLicense;

impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for BaseLicense {}

fn retry_after_header() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// `{url_prefix}/v1` without a trailing slash.
fn v1_base(url_prefix: &str) -> String {
    format!("{}/v1", url_prefix.trim_end_matches('/'))
}

/// Chat CRUD (`/chats`, `/chats/{id}`).
fn register_chat_routes(mut router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    router = OperationBuilder::post(format!("{base}/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([BaseLicense])
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
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
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([BaseLicense])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::chats::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of chats",
        )
        .with_odata_filter::<ChatDetailDtoFilterField>()
        .with_odata_orderby::<ChatDetailDtoFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get chat metadata and message count")
        .tag(TAG_CHATS)
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::chats::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::patch(format!("{base}/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update chat title")
        .tag(TAG_CHATS)
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features([BaseLicense])
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
        .response_header(retry_after_header())
        .register(router, openapi);

    OperationBuilder::delete(format!("{base}/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::chats::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

/// Messages list, `messages:stream`, turn status and turn mutations.
fn register_message_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    router = OperationBuilder::get(format!("{base}/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::messages::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of messages",
        )
        .with_odata_filter::<MiniChatMessageDtoFilterField>()
        .with_odata_orderby::<MiniChatMessageDtoFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    // `messages:stream` is a literal path segment (axum 0.8 treats `:` as
    // a plain character; `tests/streaming.rs` pins the route).
    router = OperationBuilder::post(format!("{base}/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(TAG_MESSAGES)
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream::stream_message)
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
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TAG_TURNS)
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::turns::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    register_turn_mutation_routes(router, openapi, base)
}

/// Retry / edit / delete of the latest turn (D§3.9).
fn register_turn_mutation_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    router = OperationBuilder::post(format!("{base}/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TAG_TURNS)
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::turns::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::patch(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TAG_TURNS)
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .json_request::<EditTurnRequest>(openapi, "Replacement user message")
        .handler(handlers::turns::edit_turn)
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
        .response_header(retry_after_header())
        .register(router, openapi);

    OperationBuilder::delete(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag(TAG_TURNS)
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::turns::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

/// Attachment upload / get / delete.
fn register_attachment_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    // No route-level body limit: the handler reads the raw body (the
    // api-gateway `defaults.body_limit_bytes` is the outer cap).
    router = OperationBuilder::post(format!("{base}/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .multipart_file_request("file", Some("File to upload"))
        .handler(handlers::attachments::upload_attachment)
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
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::attachments::get_attachment)
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
        .response_header(retry_after_header())
        .register(router, openapi);

    OperationBuilder::delete(format!("{base}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG_ATTACHMENTS)
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::attachments::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

/// Set / remove the caller's reaction on an assistant message.
fn register_reaction_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    router = OperationBuilder::put(format!("{base}/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set a reaction on a message")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(handlers::reactions::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    OperationBuilder::delete(format!("{base}/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag(TAG_REACTIONS)
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::reactions::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

/// Register all mini-chat routes and attach `Extension<Arc<AppServices>>`.
pub fn register_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<AppServices>,
) -> Router {
    let base = v1_base(&state.config.url_prefix);
    router = register_chat_routes(router, openapi, &base);
    router = register_message_routes(router, openapi, &base);
    router = register_attachment_routes(router, openapi, &base);
    router = register_reaction_routes(router, openapi, &base);

    router = OperationBuilder::get(format!("{base}/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::models::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(TAG_MODELS)
        .path_param("id", "Model identifier")
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::models::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::quota::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router.layer(Extension(state))
}
