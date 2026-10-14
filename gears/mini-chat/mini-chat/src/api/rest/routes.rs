//! REST route registration under the gear URL prefix.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Router};
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit::api::{OpenApiRegistry, ThrottlingSpec};
use toolkit_odata::Page;

use super::dto::{
    ChatDetailDto, ChatDtoFilterField, CreateChatReq, EditTurnRequest, MessageDtoFilterField,
    MiniChatMessageDto, MiniChatReactionDto, ModelDto, ModelListDto, QuotaStatusResponseDto,
    SetReactionReq, StreamMessageRequest, StreamStartedData, TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::domain::services::Services;

const MODELS_TAG: &str = "Mini Chat Models";
const CHATS_TAG: &str = "Mini Chat Chats";
const QUOTAS_TAG: &str = "Mini Chat Quotas";
const MESSAGES_TAG: &str = "Mini Chat Messages";
const TURNS_TAG: &str = "Mini Chat Turns";
const REACTIONS_TAG: &str = "Mini Chat Reactions";

/// The api-gateway throttling zones of `POST /v1/chats` (defined in the gateway configuration).
const CHAT_RATE_LIMIT_ZONE: &str = "rl_mini_chat_chat";
const CHAT_IN_FLIGHT_ZONE: &str = "ifl_mini_chat_chat";

/// The base license feature every route declares; the api-gateway license middleware enforces
/// it before any handler runs.
// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-license-gate:p1:inst-license-declare
struct BaseLicense;

impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for BaseLicense {}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-license-gate:p1:inst-license-declare

/// The `Retry-After` header of a 503 response.
fn retry_after_header() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// Register every route of the gear under `url_prefix` (default `/mini-chat`): as
/// authenticated operations with the base license feature and the standard errors
/// declared. The gear foundation owns `GET /v1/models` and `GET /v1/models/{id}`; chat
/// management owns the five chat operations; the other features add their endpoints here. The
/// deferred MCP endpoints and `POST /v1/chats/{id}:temporary` are not registered.
///
/// The services are attached as a request extension after all routes.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    url_prefix: &str,
    services: Arc<Services>,
) -> Router {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-shared-surface
    // every feature registers its endpoints of the DESIGN `Endpoints Overview` in this step
    let router = register_models(router, openapi, url_prefix);
    let router = register_chats(router, openapi, url_prefix);
    let router = register_messages(router, openapi, url_prefix);
    let router = quota_status_route(router, openapi, url_prefix);
    let router = register_attachments(router, openapi, url_prefix);
    let router = register_mutations(router, openapi, url_prefix);
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-shared-surface
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-return
    router.layer(Extension(services))
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-return
}

/// `GET /v1/models` and `GET /v1/models/{id}`: the endpoints owned by the gear foundation.
fn register_models(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-models
    let router = list_models_route(router, openapi, url_prefix);
    get_model_route(router, openapi, url_prefix)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-models
}

fn list_models_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-prefix
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-request
    let path = format!("{url_prefix}/v1/models");
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-request
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-prefix
    OperationBuilder::get(path)
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .tag(MODELS_TAG)
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-authn
        .authenticated()
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-authn
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-license
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-license
        .require_license_features::<BaseLicense>([BaseLicense])
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-license
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-license
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-openapi
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-unauthenticated
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-401
        .error_401(openapi)
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-401
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-unauthenticated
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-unlicensed
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-license-403
        .error_403(openapi)
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-license-403
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-unlicensed
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-openapi
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-openapi
        .register(router, openapi)
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-route-registration:p1:inst-routes-openapi
}

fn get_model_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::get(format!("{url_prefix}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .tag(MODELS_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Model identifier")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

/// The five chat operations: `POST` and `GET /v1/chats`, `GET`, `PATCH` and `DELETE`
/// `/v1/chats/{id}`.
fn register_chats(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    let router = create_chat_route(router, openapi, url_prefix);
    let router = list_chats_route(router, openapi, url_prefix);
    let router = get_chat_route(router, openapi, url_prefix);
    let router = update_chat_route(router, openapi, url_prefix);
    delete_chat_route(router, openapi, url_prefix)
}

fn create_chat_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::post(format!("{url_prefix}/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(CHATS_TAG)
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-authn-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-401
        // api-gateway authentication answers 401 before the handler
        .authenticated()
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-401
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-authn-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-license
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-unlicensed
        // @cpt-begin:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-license-403
        // the license gate of the foundation answers 403 `LICENSE_FEATURE_REQUIRED`
        .require_license_features::<BaseLicense>([BaseLicense])
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-return-license-403
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-unlicensed
        // @cpt-end:cpt-cf-mini-chat-flow-chat-management-create-chat:p1:inst-create-license
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some(CHAT_RATE_LIMIT_ZONE.to_owned()),
            in_flight_limit_zone: Some(CHAT_IN_FLIGHT_ZONE.to_owned()),
            require_security_context: false,
            dry_run: false,
        })
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
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
        .register(router, openapi)
}

fn list_chats_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::get(format!("{url_prefix}/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        // `$select` is accepted and validated by the extractor but not declared
        .with_odata_filter::<ChatDtoFilterField>()
        .with_odata_orderby::<ChatDtoFilterField>()
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(openapi, StatusCode::OK, "List of chats")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

fn get_chat_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::get(format!("{url_prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat details")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

fn update_chat_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::patch(format!("{url_prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat updated")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

fn delete_chat_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::delete(format!("{url_prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::delete_chat)
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

/// The message-streaming operations of slice S1: the messages list, the send endpoint and the
/// turn status.
fn register_messages(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-openapi
    // 200 (`text/event-stream`), 400, 401, 403, 404, 409, 422, 429, 500 and 503 for
    // `messages:stream`; 200, 400, 401, 403, 404, 500 and 503 for the list and the turn status;
    // 415 is not declared
    let router = list_messages_route(router, openapi, url_prefix);
    let router = stream_message_route(router, openapi, url_prefix);
    get_turn_route(router, openapi, url_prefix)
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-openapi
}

fn list_messages_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::get(format!("{url_prefix}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(MESSAGES_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<MessageDtoFilterField>()
        .with_odata_orderby::<MessageDtoFilterField>()
        .handler(handlers::list_messages)
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
        .response_header(retry_after_header())
        .register(router, openapi)
}

fn stream_message_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::post(format!("{url_prefix}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .tag(MESSAGES_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream_message)
        .sse_json::<StreamStartedData>(openapi, "SSE stream of chat response events")
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
        .register(router, openapi)
}

fn get_turn_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::get(format!("{url_prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

/// The turn mutation and reaction operations (FEATURE 0006): retry, edit and delete of the latest
/// terminal turn and the set and remove reaction endpoints. 415 is not declared.
fn register_mutations(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-openapi
    // the declared statuses: retry 200 (`text/event-stream`), 400, 401, 403, 404, 409, 429, 500, 503;
    // edit the same plus 422; delete 204, 400, 401, 403, 404, 409, 500, 503; set reaction 200, 400,
    // 401, 403, 404, 422, 500, 503; remove reaction 204, 400, 401, 403, 404, 500, 503; 415 is not
    // declared
    let router = OperationBuilder::post(format!(
        "{url_prefix}/v1/chats/{{id}}/turns/{{request_id}}/retry"
    ))
    .operation_id("mini_chat.retry_turn")
    .summary("Retry the last turn and stream the new answer")
    .tag(TURNS_TAG)
    .authenticated()
    .require_license_features::<BaseLicense>([BaseLicense])
    .path_param("id", "Chat UUID")
    .path_param("request_id", "Turn request UUID")
    .handler(handlers::retry_turn)
    .sse_json::<StreamStartedData>(openapi, "SSE stream of the new chat response events")
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
    let router =
        OperationBuilder::patch(format!("{url_prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
            .operation_id("mini_chat.edit_turn")
            .summary("Edit the last turn and stream the new answer")
            .tag(TURNS_TAG)
            .authenticated()
            .require_license_features::<BaseLicense>([BaseLicense])
            .path_param("id", "Chat UUID")
            .path_param("request_id", "Turn request UUID")
            .json_request::<EditTurnRequest>(openapi, "The new content of the last user message")
            .handler(handlers::edit_turn)
            .sse_json::<StreamStartedData>(openapi, "SSE stream of the new chat response events")
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
    let router =
        OperationBuilder::delete(format!("{url_prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
            .operation_id("mini_chat.delete_turn")
            .summary("Delete the last turn")
            .tag(TURNS_TAG)
            .authenticated()
            .require_license_features::<BaseLicense>([BaseLicense])
            .path_param("id", "Chat UUID")
            .path_param("request_id", "Turn request UUID")
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
    let router = OperationBuilder::put(format!(
        "{url_prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"
    ))
    .operation_id("mini_chat.put_reaction")
    .summary("Set a like or dislike reaction on an assistant message")
    .tag(REACTIONS_TAG)
    .authenticated()
    .require_license_features::<BaseLicense>([BaseLicense])
    .path_param("id", "Chat UUID")
    .path_param("msg_id", "Message UUID")
    .json_request::<SetReactionReq>(openapi, "The reaction")
    .handler(handlers::put_reaction)
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
    OperationBuilder::delete(format!(
        "{url_prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction"
    ))
    .operation_id("mini_chat.delete_reaction")
    .summary("Remove the reaction on an assistant message")
    .tag(REACTIONS_TAG)
    .authenticated()
    .require_license_features::<BaseLicense>([BaseLicense])
    .path_param("id", "Chat UUID")
    .path_param("msg_id", "Message UUID")
    .handler(handlers::delete_reaction)
    .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
    .error_400(openapi)
    .error_401(openapi)
    .error_403(openapi)
    .error_404(openapi)
    .error_500(openapi)
    .error_503(openapi)
    .response_header(retry_after_header())
    .register(router, openapi)

    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-openapi
}

/// `POST /v1/chats/{id}/attachments`, `GET /v1/chats/{id}/attachments/{attachment_id}` and
/// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
///
/// The upload declares the `multipart/form-data` request with the `file` field; a body above the
/// api-gateway `defaults.body_limit_bytes` is refused by the gateway with 413.
fn register_attachments(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    let router = OperationBuilder::post(format!("{url_prefix}/v1/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag("Mini Chat Attachments")
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        .path_param("id", "Chat UUID")
        .multipart_file_request("file", Some("File to upload"))
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<super::dto::AttachmentDetailDto>(
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
    let router = OperationBuilder::get(format!(
        "{url_prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}"
    ))
    .operation_id("mini_chat.get_attachment")
    .summary("Get attachment metadata")
    .tag("Mini Chat Attachments")
    .authenticated()
    .require_license_features::<BaseLicense>([BaseLicense])
    .path_param("id", "Chat UUID")
    .path_param("attachment_id", "Attachment UUID")
    .handler(handlers::get_attachment)
    .json_response_with_schema::<super::dto::AttachmentDetailDto>(
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
    OperationBuilder::delete(format!(
        "{url_prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}"
    ))
    .operation_id("mini_chat.delete_attachment")
    .summary("Delete an attachment")
    .tag("Mini Chat Attachments")
    .authenticated()
    .require_license_features::<BaseLicense>([BaseLicense])
    .path_param("id", "Chat UUID")
    .path_param("attachment_id", "Attachment UUID")
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
    .register(router, openapi)
}

/// `GET /v1/quota/status`: the quota status of the authenticated user.
fn quota_status_route(router: Router, openapi: &dyn OpenApiRegistry, url_prefix: &str) -> Router {
    OperationBuilder::get(format!("{url_prefix}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .tag(QUOTAS_TAG)
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-request
        // no query parameters; the bearer token and the base license feature are enforced by the
        // api-gateway before the handler, which evaluates `read` on `UserQuota`
        .authenticated()
        .require_license_features::<BaseLicense>([BaseLicense])
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-request
        .handler(handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponseDto>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi)
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;
