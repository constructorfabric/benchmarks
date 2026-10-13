//! REST route registration through the platform `OperationBuilder`.
//!
//! Every route is authenticated and gated by the platform base license
//! feature (DESIGN "License Gate": interim feature until the license plugin
//! exposes `ai_chat`), enforced by the api-gateway license middleware.
//! Error responses use the canonical `Problem` schema; every operation
//! declares the 503 `Retry-After` header (PDP unavailable).
//!
//! The attachment upload declares a `multipart/form-data` body but no
//! gateway Content-Type whitelist: the handler resolves the multipart
//! boundary itself and answers `BOUNDARY_REQUIRED` for any other content
//! type (DESIGN "Upload Attachment" error table), see
//! [`HandlerValidatedContentType`].
//!
//! `POST {prefix}/v1/chats` is bound to the api-gateway throttling zones
//! [`CREATE_CHAT_RATE_LIMIT_ZONE`] and [`CREATE_CHAT_IN_FLIGHT_LIMIT_ZONE`]
//! (limits live in the gateway configuration).

use axum::{Extension, Router};
use toolkit::api::canonical_prelude::StatusCode;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder, OperationBuilderODataExt,
    ResponseHeaderSpec, ResponseHeaderType, ThrottlingSpec,
};
use toolkit::api::{OpenApiRegistry, OperationSpec};
use toolkit_odata::Page;
use utoipa::openapi::{RefOr, Schema};

use super::RestState;
use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatReq, EditTurnRequest, MessageDto,
    MiniChatSseEvent, ModelDto, ModelListDto, QuotaStatusResponse, ReactionDto, SetReactionReq,
    StreamMessageRequest, TurnStatusResponse, UpdateChatReq,
};
use super::handlers;
use crate::infra::db::repo::odata::{ChatField, MessageField};

/// `OpenAPI` tag of the Models API.
pub const MODELS_TAG: &str = "Mini Chat Models";
/// `OpenAPI` tag of the chat CRUD operations.
pub const CHATS_TAG: &str = "Mini Chat Chats";
/// `OpenAPI` tag of the message history.
pub const MESSAGES_TAG: &str = "Mini Chat Messages";
/// `OpenAPI` tag of the reaction operations.
pub const REACTIONS_TAG: &str = "Mini Chat Reactions";
/// `OpenAPI` tag of the turn operations.
pub const TURNS_TAG: &str = "Mini Chat Turns";
/// `OpenAPI` tag of the quota operations.
pub const QUOTAS_TAG: &str = "Mini Chat Quotas";
/// `OpenAPI` tag of the attachment operations.
pub const ATTACHMENTS_TAG: &str = "Mini Chat Attachments";

/// Gateway rate-limit zone of `POST /v1/chats`.
pub const CREATE_CHAT_RATE_LIMIT_ZONE: &str = "rl_mini_chat_chat";
/// Gateway in-flight-limit zone of `POST /v1/chats`.
pub const CREATE_CHAT_IN_FLIGHT_LIMIT_ZONE: &str = "ifl_mini_chat_chat";

/// License feature required by every mini-chat route.
#[derive(Debug, Clone, Copy)]
pub struct BaseLicense;

impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for BaseLicense {}

/// `Retry-After` header of the 503 responses (PDP unavailable).
#[must_use]
pub fn retry_after_header() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Retry-After",
        "Seconds to wait before retrying",
        ResponseHeaderType::Integer,
    )
}

/// `Location` header of the 201 create response.
#[must_use]
pub fn location_header() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new(
        "Location",
        "Path of the created chat",
        ResponseHeaderType::String,
    )
}

/// Throttling binding of `POST /v1/chats`: IP-keyed zones applied before
/// authentication, observe-only (`dry_run`) until enforcement is enabled.
#[must_use]
pub fn create_chat_throttling() -> ThrottlingSpec {
    ThrottlingSpec {
        rate_limit_zone: Some(CREATE_CHAT_RATE_LIMIT_ZONE.to_owned()),
        in_flight_limit_zone: Some(CREATE_CHAT_IN_FLIGHT_LIMIT_ZONE.to_owned()),
        require_security_context: false,
        dry_run: true,
    }
}

/// Registers every mini-chat REST route under `url_prefix` (e.g.
/// `/mini-chat`) and attaches `state` once for all of them.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: RestState,
    url_prefix: &str,
) -> Router {
    let prefix = url_prefix.trim_end_matches('/');
    let router = register_models(router, openapi, prefix);
    let router = register_chats(router, openapi, prefix);
    let router = register_messages(router, openapi, prefix);
    let router = register_stream(router, openapi, prefix);
    let router = register_turn_mutations(router, openapi, prefix);
    let router = register_quota(router, openapi, prefix);
    let router = register_attachments(router, openapi, prefix);
    router.layer(Extension(state))
}

/// `GET {prefix}/v1/models` and `GET {prefix}/v1/models/{id}`.
fn register_models(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let router = OperationBuilder::get(format!("{prefix}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .description(
            "Models of the policy catalog that are globally enabled, as user-facing \
             projections (no provider, routing or credit internals).",
        )
        .tag(MODELS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    OperationBuilder::get(format!("{prefix}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .description(
            "One globally enabled catalog model. Disabled and unknown models are both \
             reported as 404 `not_found` on the model resource type.",
        )
        .tag(MODELS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
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

/// Chat CRUD: `POST` / `GET {prefix}/v1/chats` and `GET` / `PATCH` /
/// `DELETE {prefix}/v1/chats/{id}`.
fn register_chats(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let chats = format!("{prefix}/v1/chats");
    let chat = format!("{prefix}/v1/chats/{{id}}");

    let router = OperationBuilder::post(&chats)
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .tag(CHATS_TAG)
        .with_throttling(create_chat_throttling())
        .authenticated()
        .require_license_features([BaseLicense])
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Created chat")
        .response_header(location_header())
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    let router = OperationBuilder::get(&chats)
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of chats",
        )
        .with_odata_filter::<ChatField>()
        .with_odata_orderby::<ChatField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    let router = OperationBuilder::get(&chat)
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    let router = OperationBuilder::patch(&chat)
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(handlers::update_chat)
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

    OperationBuilder::delete(&chat)
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
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

/// Message history, reactions and turn status under
/// `{prefix}/v1/chats/{id}`.
fn register_messages(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let reaction = format!("{prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction");

    let router = OperationBuilder::get(format!("{prefix}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag(MESSAGES_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_messages)
        .json_response_with_schema::<Page<MessageDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of messages",
        )
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    let router = OperationBuilder::put(&reaction)
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag(REACTIONS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(handlers::put_reaction)
        .json_response_with_schema::<ReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after_header())
        .register(router, openapi);

    let router = OperationBuilder::delete(&reaction)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag(REACTIONS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
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
        .register(router, openapi);

    OperationBuilder::get(format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
        .operation_id("mini_chat.get_turn")
        .summary("Get a turn by request ID")
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
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

/// `POST {prefix}/v1/chats/{id}/messages:stream`. The last segment is the
/// literal `messages:stream` (axum 0.8 captures only `{name}` segments, so
/// the colon needs no escaping).
fn register_stream(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    OperationBuilder::post(format!("{prefix}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .description(
            "Starts a new turn and streams the answer as server-sent events \
             (`stream_started ping* (delta | tool)* citations? (done | error)`); a \
             completed turn with the same `request_id` is replayed. Every rejection \
             before the stream opens is a JSON `Problem`.",
        )
        .tag(MESSAGES_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(handlers::stream_message)
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
        .register(router, openapi)
}

/// Turn mutations (DESIGN section 3.9): `POST
/// {prefix}/v1/chats/{id}/turns/{request_id}/retry` and `PATCH` (both SSE,
/// same contract as `messages:stream`) and `DELETE
/// {prefix}/v1/chats/{id}/turns/{request_id}` (204).
fn register_turn_mutations(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let turn = format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}");

    let router = OperationBuilder::post(format!("{turn}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest terminal turn and stream the new response via SSE")
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
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

    let router = OperationBuilder::patch(&turn)
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn's user message and stream the new response via SSE")
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Turn request UUID")
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

    OperationBuilder::delete(&turn)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete a turn")
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
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
        .register(router, openapi)
}

/// `GET {prefix}/v1/quota/status`.
fn register_quota(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    OperationBuilder::get(format!("{prefix}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .description(
            "Per-tier (`premium`, `total`), per-period (`daily`, `monthly`) credit \
             usage under the user's current limits, with the remaining percentage, \
             the warning and exhausted flags and the next reset.",
        )
        .tag(QUOTAS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::get_quota_status)
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
        .register(router, openapi)
}

/// Attachments: `POST {prefix}/v1/chats/{id}/attachments` (multipart, 201)
/// and `GET` / `DELETE {prefix}/v1/chats/{id}/attachments/{attachment_id}`.
fn register_attachments(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let attachment = format!("{prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}");

    let router = OperationBuilder::post(format!("{prefix}/v1/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(ATTACHMENTS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .multipart_file_request(
            super::handlers::attachments::FILE_FIELD,
            Some("File to upload"),
        )
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
        .register(router, &HandlerValidatedContentType(openapi));

    let router = OperationBuilder::get(&attachment)
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(ATTACHMENTS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
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

    OperationBuilder::delete(&attachment)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(ATTACHMENTS_TAG)
        .authenticated()
        .require_license_features([BaseLicense])
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

/// Registry adapter for operations whose handler validates the request
/// `Content-Type` itself: forwards every registration with the gateway
/// Content-Type whitelist removed, keeping the documented request body.
///
/// `multipart_file_request` restricts the route to `multipart/form-data` at
/// the api-gateway, which would answer a missing boundary or another content
/// type with its own `UNSUPPORTED_MEDIA_TYPE` problem before mini-chat; the
/// upload contract reports these as `BOUNDARY_REQUIRED` (`content_type`).
pub struct HandlerValidatedContentType<'a>(pub &'a dyn OpenApiRegistry);

impl OpenApiRegistry for HandlerValidatedContentType<'_> {
    fn register_operation(&self, spec: &OperationSpec) {
        let mut spec = spec.clone();
        spec.allowed_request_content_types = None;
        self.0.register_operation(&spec);
    }

    fn ensure_schema_raw(&self, name: &str, schemas: Vec<(String, RefOr<Schema>)>) -> String {
        self.0.ensure_schema_raw(name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self.0.as_any()
    }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod tests;
