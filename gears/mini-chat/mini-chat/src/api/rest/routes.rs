//! Route registration (paths under the gear `url_prefix`).

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ThrottlingSpec};
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto,
    MiniChatReactionDto, MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse,
    SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::{attachments, handlers};
use crate::domain::service::MiniChat;

const TAG: &str = "Mini Chat";

/// License requirement of every route (interim base feature, ADR-0008).
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

/// Register every mini-chat route.
#[allow(clippy::too_many_lines)]
pub fn register_routes(router: Router, openapi: &dyn OpenApiRegistry, svc: Arc<MiniChat>, prefix: &str) -> Router {
    let p = |s: &str| format!("{prefix}/v1{s}");
    let lic = || [License::Base];

    let router = OperationBuilder::post(p("/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .tag(TAG)
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .authenticated()
        .require_license_features(lic())
        .json_request::<CreateChatReq>(openapi, "Chat title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Opaque cursor")
        .query_param("$filter", false, "OData filter over updated_at, id, title")
        .query_param("$orderby", false, "OData ordering over updated_at, id, title")
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Paginated list of chats")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/chats/{id}"))
        .operation_id("mini_chat.get_chat")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(p("/chats/{id}"))
        .operation_id("mini_chat.update_chat")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .json_request::<UpdateChatReq>(openapi, "New title")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/chats/{id}"))
        .operation_id("mini_chat.delete_chat")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/chats/{id}/messages"))
        .operation_id("mini_chat.list_messages")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .query_param_typed("limit", false, "Page size (default 20, max 100)", "integer")
        .query_param("cursor", false, "Opaque cursor")
        .query_param("$filter", false, "OData filter over created_at, id, role")
        .query_param("$orderby", false, "OData ordering over created_at, id, role")
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Paginated list of messages")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(p("/chats/{id}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .json_request::<StreamMessageRequest>(openapi, "Message")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat response events")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.get_turn")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(p("/chats/{id}/turns/{request_id}/retry"))
        .operation_id("mini_chat.retry_turn")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(p("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.edit_turn")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .json_request::<EditTurnRequest>(openapi, "New content")
        .handler(handlers::edit_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of the regenerated response")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/chats/{id}/turns/{request_id}"))
        .operation_id("mini_chat.delete_turn")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("request_id", "Turn request id")
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(p("/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/chats/{id}/messages/{msg_id}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::post(p("/chats/{id}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .multipart_file_request("file", Some("File to upload"))
        .handler(attachments::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Attachment uploaded and processed")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.get_attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(attachments::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment metadata")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(p("/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.delete_attachment")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .handler(attachments::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/models"))
        .operation_id("mini_chat.list_models")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/models/{id}"))
        .operation_id("mini_chat.get_model")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .path_param("id", "Model id")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(p("/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .tag(TAG)
        .authenticated()
        .require_license_features(lic())
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(svc))
}
