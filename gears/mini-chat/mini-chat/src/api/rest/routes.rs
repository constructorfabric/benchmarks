//! Route and OpenAPI registration (`OperationBuilder`).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilderODataExt};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ParamSpec, ResponseHeaderSpec, ResponseHeaderType};
use toolkit_odata::Page;

use crate::api::rest::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto, MiniChatSseEvent, ModelDto,
    ModelListDto, QuotaStatusResponse, ReactionDto, SetReactionReq, StreamMessageRequest, TurnStatusResponse,
    UpdateChatReq,
};
use crate::api::rest::handlers;
use crate::domain::odata_fields::{ChatField, MessageField};
use crate::domain::state::AppState;

const TAG: &str = "Mini Chat";

/// Interim license gate: the platform base license feature (ADR-0008).
struct BaseLicense;
impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}
impl LicenseFeature for BaseLicense {}

fn limit_param() -> ParamSpec {
    ParamSpec::query("limit")
        .param_type("integer")
        .description("Page size (default 20, max 100)")
}

fn cursor_param() -> ParamSpec {
    ParamSpec::query("cursor").description("Opaque pagination cursor")
}

#[allow(clippy::too_many_lines)]
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, state: Arc<AppState>, prefix: &str) -> Router {
    let p = prefix.trim_end_matches('/');
    let chats = format!("{p}/v1/chats");
    let chat = format!("{p}/v1/chats/{{id}}");

    let router = OperationBuilder::post(chats.clone())
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .json_request::<CreateChatReq>(openapi, "Optional title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
        .response_header(ResponseHeaderSpec::new("Location", "URI of the new chat", ResponseHeaderType::String))
        .standard_errors(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(chats.clone())
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .param(limit_param())
        .param(cursor_param())
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Chats")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(chat.clone())
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(chat.clone())
        .operation_id("mini_chat.update_chat")
        .summary("Rename a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .json_request::<UpdateChatReq>(openapi, "New title")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat")
        .standard_errors(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(chat.clone())
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{chat}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .param(limit_param())
        .param(cursor_param())
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MessageDto>>(openapi, StatusCode::OK, "Messages")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{chat}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response (SSE)")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::send_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .standard_errors(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{chat}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Attachment")
        .standard_errors(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let att = format!("{chat}/attachments/{{attachment_id}}");
    let router = OperationBuilder::get(att.clone())
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(att)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let turn = format!("{chat}/turns/{{request_id}}");
    let router = OperationBuilder::get(turn.clone())
        .operation_id("mini_chat.get_turn")
        .summary("Turn status")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::turn_status)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn status")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(format!("{turn}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the last turn (SSE)")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(turn.clone())
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the last turn (SSE)")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream")
        .standard_errors(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(turn)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the last turn")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let reaction = format!("{chat}/messages/{{msg_id}}/reaction");
    let router = OperationBuilder::put(reaction.clone())
        .operation_id("mini_chat.set_reaction")
        .summary("Set a reaction")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::set_reaction)
        .json_response_with_schema::<ReactionDto>(openapi, StatusCode::OK, "Reaction")
        .standard_errors(openapi)
        .error_415(openapi)
        .error_422(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(reaction)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Removed")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Models")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Model id")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{p}/v1/quota/status"))
        .operation_id("mini_chat.quota_status")
        .summary("Quota status")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(openapi, StatusCode::OK, "Quota status")
        .standard_errors(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(state))
}
