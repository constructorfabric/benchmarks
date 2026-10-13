//! Route registration (`OperationBuilder`, full paths `{url_prefix}/v1/...`).

use std::sync::Arc;

use axum::{Extension, Router};
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::OperationBuilderODataExt;
use toolkit::api::{
    OpenApiRegistry, OperationBuilder, ResponseHeaderSpec, ResponseHeaderType, ThrottlingSpec,
};
use toolkit_odata::Page;

use crate::api::dto::attachments::AttachmentDetailDto;
use crate::api::dto::chats::{ChatDetailDto, ChatField, CreateChatReq, UpdateChatReq};
use crate::api::dto::messages::{MessageField, MiniChatMessageDto};
use crate::api::dto::models::{ModelDto, ModelListDto};
use crate::api::dto::quota::QuotaStatusResponse;
use crate::api::dto::reactions::{MiniChatReactionDto, SetReactionReq};
use crate::api::dto::stream::{MiniChatSseEvent, StreamMessageRequest};
use crate::api::dto::turns::{EditTurnRequest, TurnStatusResponse};
use crate::api::handlers;
use crate::api::license::License;
use crate::api::state::AppServices;

const TAG_ATTACHMENTS: &str = "Mini Chat Attachments";
const TAG_CHATS: &str = "Mini Chat Chats";
const TAG_MESSAGES: &str = "Mini Chat Messages";
const TAG_MODELS: &str = "Mini Chat Models";
const TAG_REACTIONS: &str = "Mini Chat Reactions";
const TAG_TURNS: &str = "Mini Chat Turns";
const TAG_QUOTAS: &str = "Mini Chat Quotas";

/// `Retry-After` header declared on every 503 response.
fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Upload, metadata and deletion of attachments.
fn register_attachment_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    // The handler reads the raw body: no route-level body limit (the api-gateway one applies).
    router = OperationBuilder::post(format!("{base}/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
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
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
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
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{base}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG_ATTACHMENTS)
        .authenticated()
        .require_license_features([&License])
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

    router
}

/// Message list, reactions, turn status and the turn mutations.
fn register_history_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    router = OperationBuilder::get(format!("{base}/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .handler(handlers::messages::list_messages)
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

    router = OperationBuilder::put(format!("{base}/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
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
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{base}/chats/{{id}}/messages/{{msg_id}}/reaction"))
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag(TAG_REACTIONS)
        .authenticated()
        .require_license_features([&License])
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

    router = OperationBuilder::get(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::turns::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    register_turn_mutation_routes(router, openapi, base)
}

/// Retry, edit and delete of the latest turn.
fn register_turn_mutation_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
    base: &str,
) -> Router {
    router = OperationBuilder::patch(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
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
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::delete(format!("{base}/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
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
        .register(router, openapi);

    router = OperationBuilder::post(format!("{base}/chats/{{id}}/turns/{{request_id}}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TAG_TURNS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
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
        .response_header(retry_after())
        .register(router, openapi);

    router
}

/// Registers every mini-chat operation under `{url_prefix}/v1` and installs `services` as a
/// request extension.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    services: Arc<AppServices>,
    url_prefix: &str,
) -> Router {
    let base = format!("{}/v1", url_prefix.trim_end_matches('/'));
    let mut router = router;

    router = OperationBuilder::get(format!("{base}/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
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

    router = OperationBuilder::post(format!("{base}/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
        // Observe-only until the operator enables enforcement in the gateway configuration.
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some("rl_mini_chat_chat".to_owned()),
            in_flight_limit_zone: Some("ifl_mini_chat_chat".to_owned()),
            require_security_context: false,
            dry_run: true,
        })
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
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
        .handler(handlers::chats::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::patch(format!("{base}/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
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

    router = OperationBuilder::delete(format!("{base}/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(TAG_CHATS)
        .authenticated()
        .require_license_features([&License])
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

    router = OperationBuilder::post(format!("{base}/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(TAG_MESSAGES)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Chat UUID")
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
        .response_header(retry_after())
        .register(router, openapi);

    router = register_attachment_routes(router, openapi, &base);
    router = register_history_routes(router, openapi, &base);

    router = OperationBuilder::get(format!("{base}/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([&License])
        .handler(handlers::models::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(TAG_MODELS)
        .authenticated()
        .require_license_features([&License])
        .path_param("id", "Model identifier")
        .handler(handlers::models::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    router = OperationBuilder::get(format!("{base}/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(TAG_QUOTAS)
        .authenticated()
        .require_license_features([&License])
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
        .response_header(retry_after())
        .register(router, openapi);

    router.layer(Extension(services))
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use serde_json::Value;
    use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};

    use super::register_routes;
    use crate::test_support::app::TestApp;

    const API_JSON: &str = include_str!("../../../../../docs/api/api.json");

    /// The registered operations, their schemas and their component schemas match the published
    /// contract (`docs/api/api.json`).
    #[tokio::test]
    async fn operations_match_published_contract() {
        let app = TestApp::builder().build().await;
        let openapi = OpenApiRegistryImpl::new();
        let _router: Router = register_routes(Router::new(), &openapi, app.services, "/mini-chat");
        let built = serde_json::to_value(
            openapi
                .build_openapi(&OpenApiInfo::default())
                .expect("build openapi"),
        )
        .unwrap();
        let mut published: Value = serde_json::from_str(API_JSON).unwrap();
        // The api-gateway adds the numeric limits of the configured throttling zones when it
        // publishes the document; the gear only declares the zone names.
        for operation in published["paths"]
            .as_object_mut()
            .expect("paths")
            .values_mut()
            .flat_map(|item| item.as_object_mut().expect("path item").values_mut())
        {
            if let Some(op) = operation.as_object_mut() {
                for key in [
                    "x-in-flight-limit",
                    "x-rate-limit-burst",
                    "x-rate-limit-rps",
                ] {
                    op.remove(key);
                }
            }
        }

        for path in [
            "/mini-chat/v1/chats",
            "/mini-chat/v1/chats/{id}",
            "/mini-chat/v1/chats/{id}/attachments",
            "/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "/mini-chat/v1/chats/{id}/messages",
            "/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
            "/mini-chat/v1/chats/{id}/messages:stream",
            "/mini-chat/v1/chats/{id}/turns/{request_id}",
            "/mini-chat/v1/chats/{id}/turns/{request_id}/retry",
            "/mini-chat/v1/models",
            "/mini-chat/v1/models/{id}",
            "/mini-chat/v1/quota/status",
        ] {
            assert_eq!(built["paths"][path], published["paths"][path], "{path}");
        }
        for schema in [
            "ChatDetailDto",
            "CreateChatReq",
            "UpdateChatReq",
            "Page_ChatDetailDto",
            "ModelDto",
            "ModelListDto",
            "ModelTierDto",
            "QuotaStatusResponse",
            "QuotaTierStatus",
            "QuotaPeriodStatus",
            "QuotaTier",
            "QuotaPeriod",
            "StreamMessageRequest",
            "WebSearchConfig",
            "MiniChatSseEvent",
            "StreamStartedData",
            "ThreadSummaryInfo",
            "PingData",
            "DeltaData",
            "DeltaKind",
            "ToolData",
            "ToolPhase",
            "CitationsData",
            "Citation",
            "CitationSource",
            "TextSpan",
            "DoneData",
            "QuotaDecisionKind",
            "QuotaWarning",
            "Usage",
            "ErrorData",
            "MiniChatMessageDto",
            "Page_MiniChatMessageDto",
            "MessageRoleDto",
            "AttachmentSummaryDto",
            "AttachmentDetailDto",
            "AttachmentKindDto",
            "AttachmentStatusDto",
            "ImgThumbnailDto",
            "ReactionKindDto",
            "SetReactionReq",
            "MiniChatReactionDto",
            "TurnStatusResponse",
            "TurnStatusState",
            "EditTurnRequest",
            "PageInfo",
        ] {
            assert_eq!(
                built["components"]["schemas"][schema], published["components"]["schemas"][schema],
                "{schema}"
            );
        }
    }
}
