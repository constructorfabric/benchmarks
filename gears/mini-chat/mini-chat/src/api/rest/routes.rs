//! Route registration (`OperationBuilder`). Every route lives under the configured `url_prefix`
//! and requires the platform base license feature.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{
    LicenseFeature, OperationBuilderODataExt, ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit::api::{OpenApiRegistry, OperationBuilder, ThrottlingSpec};

use super::dto::{
    AttachmentDetailDto, ChatDetailDto, CreateChatRequest, EditTurnRequest, MessageDto, ModelDto,
    ModelListResponse, QuotaStatusDto, ReactionDto, SendMessageRequest, SetReactionRequest,
    StreamEventDoc, TurnStatusDto, UpdateChatRequest,
};
use super::handlers;
use crate::domain::attachment::AttachmentService;
use crate::domain::chat::{ChatFilterField, ChatService};
use crate::domain::message::{MessageFilterField, MessageService};
use crate::domain::models::ModelsService;
use crate::domain::mutation::TurnMutationService;
use crate::domain::quota::QuotaStatusService;
use crate::domain::stream::StreamService;
use crate::domain::turn::TurnService;

const TAG: &str = "Mini Chat";

/// The platform base license feature every mini-chat route requires until the license plugin
/// exposes `ai_chat` (ADR-0008).
const BASE_LICENSE_FEATURE: &str =
    toolkit::api::operation_builder::CORE_GLOBAL_BASE_LICENSE_FEATURE;

/// Rate-limit zone of the create-chat route.
const CREATE_CHAT_RATE_ZONE: &str = "rl_mini_chat_chat";
/// In-flight-limit zone of the create-chat route.
const CREATE_CHAT_IN_FLIGHT_ZONE: &str = "ifl_mini_chat_chat";

struct BaseLicense;

impl AsRef<str> for BaseLicense {
    fn as_ref(&self) -> &str {
        BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for BaseLicense {}

/// The services the routes serve.
pub struct ApiServices {
    /// Chat CRUD.
    pub chats: Arc<ChatService>,
    /// Models API.
    pub models: Arc<ModelsService>,
    /// Messages API and reactions.
    pub messages: Arc<MessageService>,
    /// Turn status API.
    pub turns: Arc<TurnService>,
    /// The send-message pipeline.
    pub stream: Arc<StreamService>,
    /// Retry, edit and delete of the latest turn.
    pub mutations: Arc<TurnMutationService>,
    /// The quota status API.
    pub quota: Arc<QuotaStatusService>,
    /// Attachment upload, get and delete.
    pub attachments: Arc<AttachmentService>,
}

/// Registers the chat, message, attachment, turn (status and mutations), model and quota routes
/// under `url_prefix` and attaches the services.
pub fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    url_prefix: &str,
    services: ApiServices,
) -> Router {
    let router = register_chat_routes(router, openapi, url_prefix);
    let router = register_message_routes(router, openapi, url_prefix);
    let router = register_mutation_routes(router, openapi, url_prefix);
    let router = register_attachment_routes(router, openapi, url_prefix);
    let router = register_model_routes(router, openapi, url_prefix);
    let router = register_quota_routes(router, openapi, url_prefix);
    router
        .layer(axum::Extension(services.chats))
        .layer(axum::Extension(services.models))
        .layer(axum::Extension(services.messages))
        .layer(axum::Extension(services.turns))
        .layer(axum::Extension(services.stream))
        .layer(axum::Extension(services.mutations))
        .layer(axum::Extension(services.quota))
        .layer(axum::Extension(services.attachments))
}

fn register_chat_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let chats_path = format!("{prefix}/v1/chats");
    let chat_path = format!("{prefix}/v1/chats/{{id}}");

    let router = OperationBuilder::post(&chats_path)
        .operation_id("mini_chat.create_chat")
        .summary("Create a chat")
        .description(
            "Creates a chat owned by the caller. The model defaults to the first enabled default \
             model and is locked for the lifetime of the chat.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some(CREATE_CHAT_RATE_ZONE.to_owned()),
            in_flight_limit_zone: Some(CREATE_CHAT_IN_FLIGHT_ZONE.to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .json_request::<CreateChatRequest>(openapi, "Optional title and model")
        .handler(handlers::create_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::CREATED, "Chat created")
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "Path of the new chat",
            ResponseHeaderType::String,
        ))
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(&chats_path)
        .operation_id("mini_chat.list_chats")
        .summary("List chats")
        .description(
            "Lists the caller's chats, most recently active first. Supports `$filter` and \
             `$orderby` over `updated_at`, `id` and `title`, `limit` (default 20, max 100) and \
             `cursor`.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::list_chats)
        .json_response_with_schema::<toolkit_odata::Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Page of chats",
        )
        .with_odata_filter::<ChatFilterField>()
        .with_odata_orderby::<ChatFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(&chat_path)
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat")
        .description("Returns the chat metadata and message count (no embedded messages).")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .handler(handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "The chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(&chat_path)
        .operation_id("mini_chat.update_chat")
        .summary("Update the chat title")
        .description("Renames the chat. Only `title` is read; other fields are ignored.")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .json_request::<UpdateChatRequest>(openapi, "The new title")
        .handler(handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "The updated chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .error_422(openapi)
        .register(router, openapi);

    OperationBuilder::delete(&chat_path)
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .description(
            "Soft-deletes the chat; provider resources are cleaned up asynchronously. A second \
             delete returns 404.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .handler(handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_model_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let models_path = format!("{prefix}/v1/models");
    let model_path = format!("{prefix}/v1/models/{{id}}");

    let router = OperationBuilder::get(&models_path)
        .operation_id("mini_chat.list_models")
        .summary("List models")
        .description("Lists the globally enabled models of the policy catalog.")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListResponse>(openapi, StatusCode::OK, "Visible models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::get(&model_path)
        .operation_id("mini_chat.get_model")
        .summary("Get a model")
        .description("Returns one globally enabled model; disabled and unknown models are 404.")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Model identifier")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "The model")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_quota_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    OperationBuilder::get(format!("{prefix}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get the quota status")
        .description(
            "Returns the caller's remaining quota per tier (`premium`, `total`) and period \
             (`daily`, `monthly`): limit, used (spent plus reserved) and remaining credits, the \
             remaining percentage, the next reset and the warning and exhausted flags. Periods \
             with a non-positive limit are omitted.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .handler(handlers::quota_status)
        .json_response_with_schema::<QuotaStatusDto>(openapi, StatusCode::OK, "The quota status")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_message_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let stream_path = format!("{prefix}/v1/chats/{{id}}/messages:stream");
    let messages_path = format!("{prefix}/v1/chats/{{id}}/messages");
    let turn_path = format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}");
    let reaction_path = format!("{prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction");

    let router = OperationBuilder::post(&stream_path)
        .operation_id("mini_chat.send_message")
        .summary("Send a message and stream the answer")
        .description(
            "Sends a user message and streams the assistant answer as server-sent events: \
             `stream_started`, `delta`, `tool`, `citations`, `done`, `error` and `ping`. \
             Validation, authorization and quota failures are normal JSON errors; once the stream \
             has started, failures are terminal `error` events. A repeated `request_id` of a \
             completed turn is replayed without a provider call.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .json_request::<SendMessageRequest>(openapi, "The message")
        .handler(handlers::send_message)
        .sse_json::<StreamEventDoc>(openapi, "The event stream of the turn")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(&messages_path)
        .operation_id("mini_chat.list_messages")
        .summary("List messages")
        .description(
            "Lists the messages of the chat in chronological order. Supports `$filter` and \
             `$orderby` over `created_at`, `id` and `role`, `limit` (default 20, max 100) and \
             `cursor`.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .handler(handlers::list_messages)
        .json_response_with_schema::<toolkit_odata::Page<MessageDto>>(
            openapi,
            StatusCode::OK,
            "Page of messages",
        )
        .with_odata_filter::<MessageFilterField>()
        .with_odata_orderby::<MessageFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(&turn_path)
        .operation_id("mini_chat.get_turn")
        .summary("Get the state of a turn")
        .description(
            "Returns the authoritative state of a turn: `running`, `done`, `error` or `cancelled`. \
             A soft-deleted turn and a turn of another chat are 404.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("request_id", "Request identifier of the turn")
        .handler(handlers::get_turn)
        .json_response_with_schema::<TurnStatusDto>(openapi, StatusCode::OK, "The turn state")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(&reaction_path)
        .operation_id("mini_chat.set_reaction")
        .summary("Set a reaction")
        .description("Sets or replaces the caller's like or dislike on an assistant message.")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("msg_id", "Message identifier")
        .json_request::<SetReactionRequest>(openapi, "The reaction")
        .handler(handlers::set_reaction)
        .json_response_with_schema::<ReactionDto>(openapi, StatusCode::OK, "The reaction")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete(&reaction_path)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction")
        .description("Removes the caller's reaction from an assistant message; idempotent.")
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("msg_id", "Message identifier")
        .handler(handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_mutation_routes(router: Router, openapi: &dyn OpenApiRegistry, prefix: &str) -> Router {
    let turn_path = format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}");
    let retry_path = format!("{prefix}/v1/chats/{{id}}/turns/{{request_id}}/retry");

    let router = OperationBuilder::post(&retry_path)
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the latest turn")
        .description(
            "Replaces the latest turn by a new turn with the original message and attachments and \
             streams the new answer like `messages:stream`. The new `request_id` is announced by \
             `stream_started`. A rejection before the mutation commits leaves the previous turn \
             untouched; a setup failure after it marks the new turn failed and returns a JSON \
             error.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("request_id", "Request identifier of the turn to retry")
        .handler(handlers::retry_turn)
        .sse_json::<StreamEventDoc>(openapi, "The event stream of the new turn")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(&turn_path)
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the latest turn")
        .description(
            "Replaces the latest turn by a new turn with the edited message and the original \
             attachments and streams the new answer like `messages:stream`.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("request_id", "Request identifier of the turn to edit")
        .json_request::<EditTurnRequest>(openapi, "The new message content")
        .handler(handlers::edit_turn)
        .sse_json::<StreamEventDoc>(openapi, "The event stream of the new turn")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_422(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete(&turn_path)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the latest turn")
        .description(
            "Soft-deletes the latest turn and its messages. No new turn is created and the \
             provider is not called.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("request_id", "Request identifier of the turn to delete")
        .handler(handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}

fn register_attachment_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    prefix: &str,
) -> Router {
    let attachments_path = format!("{prefix}/v1/chats/{{id}}/attachments");
    let attachment_path = format!("{prefix}/v1/chats/{{id}}/attachments/{{attachment_id}}");

    let router = OperationBuilder::post(&attachments_path)
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .description(
            "Uploads a document or an image to the chat. The body is `multipart/form-data` with a \
             `file` part that carries a content type; the route does not restrict the request \
             content type, a body without a multipart boundary is a 400 `BOUNDARY_REQUIRED`. \
             The file is stored with the RAG provider; a document is added to the chat vector \
             store and an image gets a thumbnail. 201 with the attachment in status `ready`, or \
             `uploaded` when indexing is still running 25 s after the upload started (poll the \
             attachment until it is `ready` or `failed`). The request body is capped by the \
             api-gateway body limit, not by this route.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .handler(handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            openapi,
            StatusCode::CREATED,
            "Attachment uploaded",
        )
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "Path of the new attachment",
            ResponseHeaderType::String,
        ))
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(&attachment_path)
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .description(
            "Returns the status and metadata of an attachment of the caller's chat. Another \
             user's attachment, another chat's and a soft-deleted one are 404.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("attachment_id", "Attachment identifier")
        .handler(handlers::get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "The attachment")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete(&attachment_path)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .description(
            "Soft-deletes an attachment that no submitted message references and schedules the \
             provider file cleanup. Repeating the call is a no-op; an attachment referenced by a \
             message is 409.",
        )
        .tag(TAG)
        .authenticated()
        .require_license_features([BaseLicense])
        .path_param("id", "Chat identifier")
        .path_param("attachment_id", "Attachment identifier")
        .handler(handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi)
}
