//! REST route registration (paths under the configured `url_prefix`).

use axum::http::StatusCode;
use axum::{Extension, Router};
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt,
};
use toolkit::api::{
    OpenApiRegistry, OperationBuilder, ParamSpec, ResponseHeaderSpec, ResponseHeaderType,
    ThrottlingSpec,
};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, ChatQueryFilterField, CreateChatReq, EditTurnRequest,
    MessageQueryFilterField, MiniChatMessageDto, MiniChatReactionDto, MiniChatSseEvent, ModelDto,
    ModelListDto, QuotaStatusResponse, SetReactionReq, StreamMessageRequest, TurnStatusResponse,
    UpdateChatReq,
};
use super::handlers::{attachments, chats, messages, models, quota, reactions, stream, turns};
use crate::config::MiniChatConfig;
use crate::domain::services::Services;

const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_QUOTAS: &str = "Mini Chat Quotas";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";

/// License features of the gear's routes (DESIGN §2.2 License Gate).
#[derive(Debug, Clone, Copy)]
pub enum License {
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

/// Throttling zones bound to `create_chat` (enforced by the api-gateway config).
fn create_chat_throttling() -> ThrottlingSpec {
    ThrottlingSpec {
        rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
        in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
        require_security_context: false,
        dry_run: true,
    }
}

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Register every route of the gear and attach `services` as a request extension.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    services: Services,
    cfg: &MiniChatConfig,
) -> Router {
    let base = format!("{}/v1", cfg.url_prefix.trim_end_matches('/'));
    let router = register_chat_routes(router, openapi, &base);
    let router = register_message_routes(router, openapi, &base);
    let router = register_model_routes(router, openapi, &base);
    let router = register_quota_routes(router, openapi, &base);
    let router = register_stream_routes(router, openapi, &base);
    let router = register_turn_routes(router, openapi, &base);
    let router = register_attachment_routes(router, openapi, &base);
    router.layer(Extension(services))
}

fn register_chat_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    let chats_path = format!("{base}/chats");
    let chat_path = format!("{base}/chats/{{id}}");

    let router = OperationBuilder::post(&chats_path)
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License::Base])
        .with_throttling(create_chat_throttling())
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(chats::create_chat)
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

    let router = OperationBuilder::get(&chats_path)
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License::Base])
        .param(
            ParamSpec::query("limit")
                .param_type("integer")
                .description("Maximum number of chats to return"),
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<ChatQueryFilterField>()
        .with_odata_orderby::<ChatQueryFilterField>()
        .handler(chats::list_chats)
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

    let router = OperationBuilder::get(&chat_path)
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .handler(chats::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::patch(&chat_path)
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(chats::update_chat)
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

    OperationBuilder::delete(&chat_path)
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .handler(chats::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_message_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    let messages_path = format!("{base}/chats/{{id}}/messages");
    let reaction_path = format!("{base}/chats/{{id}}/messages/{{msg_id}}/reaction");

    let router = OperationBuilder::get(&messages_path)
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .param(
            ParamSpec::query("limit")
                .param_type("integer")
                .description("Maximum number of messages to return"),
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<MessageQueryFilterField>()
        .with_odata_orderby::<MessageQueryFilterField>()
        .handler(messages::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(
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

    let router = OperationBuilder::put(&reaction_path)
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(reactions::put_reaction)
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

    OperationBuilder::delete(&reaction_path)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .handler(reactions::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_model_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    let models_path = format!("{base}/models");
    let model_path = format!("{base}/models/{{id}}");

    let router = OperationBuilder::get(&models_path)
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License::Base])
        .handler(models::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::get(&model_path)
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Model identifier")
        .handler(models::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn register_quota_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    OperationBuilder::get(format!("{base}/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([License::Base])
        .handler(quota::get_quota_status)
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
        .register(router, openapi)
}

fn register_stream_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    OperationBuilder::post(format!("{base}/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([License::Base])
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
        .register(router, openapi)
}

fn register_turn_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    let router = OperationBuilder::get(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(turns::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{base}/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(turns::retry_turn)
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

    let router = OperationBuilder::patch(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .json_request::<EditTurnRequest>(openapi, "Replacement user message")
        .handler(turns::edit_turn)
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

    OperationBuilder::delete(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(turns::delete_turn)
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

fn register_attachment_routes(router: Router, openapi: &dyn OpenApiRegistry, base: &str) -> Router {
    let attachments_path = format!("{base}/chats/{{id}}/attachments");
    let attachment_path = format!("{base}/chats/{{id}}/attachments/{{attachment_id}}");

    let router = OperationBuilder::post(&attachments_path)
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .multipart_file_request("file", Some("File to upload"))
        .handler(attachments::upload_attachment)
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

    let router = OperationBuilder::get(&attachment_path)
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(attachments::get_attachment)
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

    OperationBuilder::delete(&attachment_path)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([License::Base])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(attachments::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
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
