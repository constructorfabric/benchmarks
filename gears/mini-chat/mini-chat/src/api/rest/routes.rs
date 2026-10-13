//! REST route registration helpers.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-license-gate:p1

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{
    LicenseFeature, OperationBuilderODataExt, ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit::api::{OpenApiRegistry, OperationBuilder};
use toolkit_gts::gts_id;
use toolkit_odata::Page;

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MiniChatMessageDto,
    MiniChatReactionDto, MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse,
    SetReactionReq, StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::service::attachments::service::AttachmentsService;
use crate::domain::service::chats::ChatsService;
use crate::domain::service::messages::MessagesService;
use crate::domain::service::models::ModelsService;
use crate::domain::service::mutations::service::MutationService;
use crate::domain::service::quota::service::QuotaService;
use crate::domain::service::stream::pipeline::StreamService;
use crate::infra::db::odata::{ChatFilterField, MessageFilterField};

/// Base license feature required by every route of the gear (interim gate).
///
/// Declared through `require_license_features` on every route, so the platform
/// license middleware rejects an unlicensed tenant with 403
/// `LICENSE_FEATURE_REQUIRED` before the handler runs; the gear never calls
/// `check_user_license`.
pub const BASE_LICENSE_FEATURE: &str = gts_id!("cf.core.lic.feat.v1~cf.core.global.base.v1");

/// License feature marker passed to `require_license_features`.
pub struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

const TAG: &str = "Mini Chat Models";

const CHAT_TAG: &str = "Mini Chat Chats";

const QUOTA_TAG: &str = "Mini Chat Quotas";

const MESSAGES_TAG: &str = "Mini Chat Messages";

const TURNS_TAG: &str = "Mini Chat Turns";

const ATTACHMENTS_TAG: &str = "Mini Chat Attachments";

fn retry_after_header() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Registers `GET {url_prefix}/v1/models` and `GET {url_prefix}/v1/models/{id}`
/// (authenticated, base license feature, `OpenAPI` metadata).
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ModelsService>,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-register-routes
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-send-request
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-call-list
    let router = OperationBuilder::get(format!("{prefix}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .description("Lists the enabled models of the catalog in catalog order.")
        .tag(TAG)
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authn
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unauthenticated
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-401
        .authenticated()
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-401
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unauthenticated
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authn
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-license-check
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unlicensed
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-license-403
        .require_license_features::<License>([License])
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-license-403
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unlicensed
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-license-check
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "Enabled models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-call-list

    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-call-get
    let router = OperationBuilder::get(format!("{prefix}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .description("Returns one enabled model; a disabled or unknown model is 404.")
        .tag(TAG)
        .path_param("id", "Model identifier (opaque string)")
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authn
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unauthenticated
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-401
        .authenticated()
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-401
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unauthenticated
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authn
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-license-check
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unlicensed
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-license-403
        .require_license_features::<License>([License])
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-license-403
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-if-unlicensed
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-license-check
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "The model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-call-get

    router.layer(axum::Extension(service))
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-send-request
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-init:p1:inst-register-routes
}

/// Registers the eight routes of the chat CRUD feature under `url_prefix`
/// (`cpt-cf-mini-chat-dod-chat-crud-rest-routes`): `POST` and `GET`
/// `/v1/chats`, `GET`, `PATCH` and `DELETE` `/v1/chats/{id}`, `GET`
/// `/v1/chats/{id}/messages`, `PUT` and `DELETE`
/// `/v1/chats/{id}/messages/{msg_id}/reaction`; each authenticated and gated by
/// the base license feature.
// @cpt-dod:cpt-cf-mini-chat-dod-chat-crud-rest-routes:p1
pub fn register_chat_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    chats: Arc<ChatsService>,
    messages: Arc<MessagesService>,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    let chats_path = format!("{prefix}/v1/chats");
    let chat_path = format!("{prefix}/v1/chats/{{id}}");
    let messages_path = format!("{prefix}/v1/chats/{{id}}/messages");
    let reaction_path = format!("{prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction");

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-call-create
    let router = OperationBuilder::post(chats_path.clone())
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .description("Creates a chat on an enabled model; the model is locked for the chat.")
        .tag(CHAT_TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<CreateChatReq>(openapi, "Chat to create")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "The new chat")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "URL of the new chat",
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
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-create-chat:p1:inst-call-create

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-call-list-chats
    let router = OperationBuilder::get(chats_path)
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .description("Lists the caller's chats, most recently active first.")
        .tag(CHAT_TAG)
        .query_param("limit", false, "Page size, default 20, maximum 100")
        .query_param("cursor", false, "Opaque page cursor")
        .with_odata_filter::<ChatFilterField>()
        .with_odata_orderby::<ChatFilterField>()
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "Chats")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-chats:p1:inst-call-list-chats

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-call-get-chat
    let router = OperationBuilder::get(chat_path.clone())
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .description("Returns chat metadata and the message count; no messages are embedded.")
        .tag(CHAT_TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "The chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-get-chat:p1:inst-call-get-chat

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-call-rename
    let router = OperationBuilder::patch(chat_path.clone())
        .operation_id("mini_chat.rename_chat")
        .summary("Rename a chat")
        .description("Updates the title of a chat; no other field can be changed.")
        .tag(CHAT_TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<UpdateChatReq>(openapi, "New title")
        .handler(handlers::rename_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "The updated chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-rename-chat:p1:inst-call-rename

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-call-delete
    let router = OperationBuilder::delete(chat_path)
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .description("Soft-deletes a chat; provider resources are cleaned up asynchronously.")
        .tag(CHAT_TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-delete-chat:p1:inst-call-delete

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-call-list-messages
    let router = OperationBuilder::get(messages_path)
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .description("Lists the messages of a chat in chronological order.")
        .tag(CHAT_TAG)
        .path_param("id", "Chat id")
        .query_param("limit", false, "Page size, default 20, maximum 100")
        .query_param("cursor", false, "Opaque page cursor")
        .with_odata_filter::<MessageFilterField>()
        .with_odata_orderby::<MessageFilterField>()
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Messages")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-list-messages:p1:inst-call-list-messages

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-call-set-reaction
    let router = OperationBuilder::put(reaction_path.clone())
        .operation_id("mini_chat.set_reaction")
        .summary("Set a reaction")
        .description("Sets like or dislike on an assistant message.")
        .tag(CHAT_TAG)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<SetReactionReq>(openapi, "Reaction")
        .handler(handlers::set_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(
            openapi,
            StatusCode::OK,
            "The stored reaction",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-set-reaction:p1:inst-call-set-reaction

    // @cpt-begin:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-call-remove-reaction
    let router = OperationBuilder::delete(reaction_path)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .description("Removes the caller's reaction on an assistant message (idempotent).")
        .tag(CHAT_TAG)
        .path_param("id", "Chat id")
        .path_param("msg_id", "Message id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    // @cpt-end:cpt-cf-mini-chat-flow-chat-crud-remove-reaction:p1:inst-call-remove-reaction

    router
        .layer(axum::Extension(chats))
        .layer(axum::Extension(messages))
}

/// Registers `GET {url_prefix}/v1/quota/status` (authenticated, base license
/// feature; `cpt-cf-mini-chat-dod-quota-billing-quota-status`).
pub fn register_quota_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    quota: Arc<QuotaService>,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    let router = OperationBuilder::get(format!("{prefix}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get the quota status")
        .description(
            "Returns the remaining quota of the caller per tier and period, \
             with warning and exhausted flags and the next reset.",
        )
        .tag(QUOTA_TAG)
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status of the caller",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    router.layer(axum::Extension(quota))
}

/// Registers the two routes of the streaming-turns feature under `url_prefix`
/// (`cpt-cf-mini-chat-dod-streaming-turns-rest-routes`):
/// `POST /v1/chats/{id}/messages:stream` (operation `mini_chat.stream_message`,
/// 200 `text/event-stream`) and `GET /v1/chats/{id}/turns/{request_id}`
/// (operation `mini_chat.get_turn`); each authenticated and gated by the base
/// license feature.
// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-rest-routes:p1
pub fn register_stream_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    streams: Arc<StreamService>,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    let router = OperationBuilder::post(format!("{prefix}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the answer")
        .description(
            "Sends a message to a chat and streams the assistant answer as server-sent \
             events: stream_started, ping, delta, tool, citations, done or error.",
        )
        .tag(MESSAGES_TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream_message)
        .sse_json::<MiniChatSseEvent>(openapi, "Server-sent events of the turn")
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

    let router = OperationBuilder::get(format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get the status of a turn")
        .description("Returns the state of the turn of a request id.")
        .tag(TURNS_TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Request id of the turn")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(
            openapi,
            StatusCode::OK,
            "Status of the turn",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    router.layer(axum::Extension(streams))
}

// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-rest-routes:p1
/// Registers the three routes of the turn mutations feature under
/// `url_prefix` (`cpt-cf-mini-chat-dod-turn-mutations-rest-routes`): `POST
/// /v1/chats/{id}/turns/{request_id}/retry` (operation `mini_chat.retry_turn`,
/// no body, 200 `text/event-stream`), `PATCH /v1/chats/{id}/turns/{request_id}`
/// (`mini_chat.edit_turn`, `EditTurnRequest`, 200 `text/event-stream`) and
/// `DELETE /v1/chats/{id}/turns/{request_id}` (`mini_chat.delete_turn`, 204);
/// each authenticated, gated by the base license feature, tagged
/// `Mini Chat Turns`.
pub fn register_turn_mutation_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    mutations: Arc<MutationService>,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    let router = OperationBuilder::post(format!(
        "{prefix}/v1/chats/{{id}}/turns/{{request_id}}/retry"
    ))
    .operation_id("mini_chat.retry_turn")
    .summary("Retry the latest terminal turn and stream the new response via SSE")
    .description(
        "Replaces the latest terminal turn of an own chat by a new turn with the original \
         message and streams the new answer as server-sent events.",
    )
    .tag(TURNS_TAG)
    .path_param("id", "Chat id")
    .path_param("request_id", "Request id of the turn to retry")
    .authenticated()
    .require_license_features::<License>([License])
    .handler(handlers::retry_turn)
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

    let item = format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}");
    let router = OperationBuilder::patch(item.clone())
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .description(
            "Replaces the latest terminal turn of an own chat by a new turn with the edited \
             message and streams the new answer as server-sent events.",
        )
        .tag(TURNS_TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Request id of the turn to edit")
        .authenticated()
        .require_license_features::<License>([License])
        .json_request::<EditTurnRequest>(openapi, "Replacement user message")
        .handler(handlers::edit_turn)
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

    let router = OperationBuilder::delete(item)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .description("Soft-deletes the latest terminal turn of an own chat and its messages.")
        .tag(TURNS_TAG)
        .path_param("id", "Chat id")
        .path_param("request_id", "Request id of the turn to delete")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);
    router.layer(axum::Extension(mutations))
}

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-rest-routes:p1
/// Registers the three routes of the attachments feature under `url_prefix`
/// (`cpt-cf-mini-chat-dod-attachments-rest-routes`): `POST
/// /v1/chats/{id}/attachments` (operation `mini_chat.upload_attachment`,
/// `multipart/form-data` with the required binary field `file`, no body limit,
/// 201 `AttachmentDetailDto`), `GET` (`mini_chat.get_attachment`) and `DELETE`
/// (`mini_chat.delete_attachment`, 204) on `/v1/chats/{id}/attachments/{attachment_id}`;
/// each authenticated, gated by the base license feature, tagged
/// `Mini Chat Attachments`.
pub fn register_attachment_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    attachments: Arc<AttachmentsService>,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    let collection = format!("{prefix}/v1/chats/{{id}}/attachments");
    let item = format!("{prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}");

    let router = OperationBuilder::post(collection)
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .description(
            "Uploads a document or an image into an own chat: the file is validated, stored \
             with the provider and, for a searchable document, indexed before the answer.",
        )
        .tag(ATTACHMENTS_TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features::<License>([License])
        .multipart_file_request("file", Some("File to upload"))
        .handler(handlers::upload_attachment)
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

    let router = OperationBuilder::get(item.clone())
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .description("Returns the metadata and the processing status of an own attachment.")
        .tag(ATTACHMENTS_TAG)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::get_attachment)
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

    let router = OperationBuilder::delete(item)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .description(
            "Soft-deletes an own attachment that no sent message references; provider files \
             are cleaned up asynchronously (idempotent).",
        )
        .tag(ATTACHMENTS_TAG)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    router.layer(axum::Extension(attachments))
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "routes_quota_tests.rs"]
mod quota_tests;

#[cfg(test)]
#[path = "routes_models_tests.rs"]
mod models_tests;

#[cfg(test)]
#[path = "routes_chats_tests.rs"]
mod chats_tests;

#[cfg(test)]
#[path = "routes_messages_tests.rs"]
mod messages_tests;

#[cfg(test)]
#[path = "routes_reactions_tests.rs"]
mod reactions_tests;

#[cfg(test)]
#[path = "routes_chat_registration_tests.rs"]
mod chat_registration_tests;

#[cfg(test)]
#[path = "routes_chat_hardening_tests.rs"]
mod chat_hardening_tests;

#[cfg(test)]
#[path = "routes_stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "routes_stream_flow_tests.rs"]
mod stream_flow_tests;

#[cfg(test)]
#[path = "routes_stream_knowledge_tests.rs"]
mod stream_knowledge_tests;

#[cfg(test)]
#[path = "routes_turns_tests.rs"]
mod turns_tests;

#[cfg(test)]
#[path = "routes_mutations_registration_tests.rs"]
mod mutations_registration_tests;

#[cfg(test)]
#[path = "routes_mutations_tests.rs"]
mod mutations_tests;

#[cfg(test)]
#[path = "routes_mutations_replace_tests.rs"]
mod mutations_replace_tests;

#[cfg(test)]
#[path = "routes_attachments_validation_tests.rs"]
mod attachments_validation_tests;

#[cfg(test)]
#[path = "routes_attachments_get_tests.rs"]
mod attachments_get_tests;

#[cfg(test)]
#[path = "routes_attachments_upload_tests.rs"]
mod attachments_upload_tests;

#[cfg(test)]
#[path = "routes_attachments_index_tests.rs"]
mod attachments_index_tests;

#[cfg(test)]
#[path = "routes_attachments_images_tests.rs"]
mod attachments_images_tests;

#[cfg(test)]
#[path = "routes_attachments_delete_tests.rs"]
mod attachments_delete_tests;

#[cfg(test)]
#[path = "routes_attachments_registration_tests.rs"]
mod attachments_registration_tests;

#[cfg(test)]
#[path = "routes_attachments_pipeline_tests.rs"]
mod attachments_pipeline_tests;
