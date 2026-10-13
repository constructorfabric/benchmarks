//! Route registration of the Models API.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use toolkit::api::operation_builder::{CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature};
use toolkit::api::{OpenApiRegistry, OperationBuilder};

use toolkit::api::ThrottlingSpec;
use toolkit::api::operation_builder::{
    OperationBuilderODataExt, ResponseHeaderSpec, ResponseHeaderType,
};
use toolkit_odata::Page;

use super::attachment_dto::AttachmentDetailDto;
use super::attachment_handlers;
use super::chat_handlers;
use super::dto::{
    ChatDetailDto, CreateChatReq, MiniChatMessageDto, MiniChatReactionDto, ModelDto, ModelListDto,
    SetReactionReq, UpdateChatReq,
};
use super::handlers;
use super::quota_dto::QuotaStatusResponse;
use super::quota_handlers;
use super::stream_dto::{
    EditTurnRequest, MiniChatSseEvent, StreamMessageRequest, TurnStatusResponse,
};
use super::stream_handlers;
use crate::domain::attachments::AttachmentService;
use crate::domain::chats::{ChatFilterField, ChatsService};
use crate::domain::messages::{MessageFilterField, MessagesService};
use crate::domain::models::ModelsService;
use crate::domain::quota_status::QuotaStatusService;
use crate::domain::reactions::ReactionsService;
use crate::domain::streaming::StreamService;
use crate::domain::turn_mutations::TurnMutationService;
use crate::domain::turns::TurnsService;

const TAG: &str = "Mini Chat Models";
const CHATS_TAG: &str = "Mini Chat Chats";
const MESSAGES_TAG: &str = "Mini Chat Messages";
const REACTIONS_TAG: &str = "Mini Chat Reactions";
const TURNS_TAG: &str = "Mini Chat Turns";
const QUOTAS_TAG: &str = "Mini Chat Quotas";
const ATTACHMENTS_TAG: &str = "Mini Chat Attachments";

/// Throttling zones of `POST /v1/chats` (defined in the api-gateway config).
const CHAT_RATE_LIMIT_ZONE: &str = "rl_mini_chat_chat";
const CHAT_IN_FLIGHT_ZONE: &str = "ifl_mini_chat_chat";

/// License feature required by the mini-chat routes: the platform base
/// feature until the license plugin exposes `ai_chat` (DESIGN 2.2 "License
/// Gate", ADR-0008). The api-gateway license middleware grants the base
/// feature without a license plugin, so requests of an unlicensed
/// development deployment stay reachable.
pub struct MiniChatLicense;

impl AsRef<str> for MiniChatLicense {
    fn as_ref(&self) -> &str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for MiniChatLicense {}

/// Registers `GET {url_prefix}/v1/models` and `GET {url_prefix}/v1/models/{id}`.
pub fn register_models_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    service: Arc<ModelsService>,
    url_prefix: &str,
) -> Router {
    let router = OperationBuilder::get(format!("{url_prefix}/v1/models"))
        .operation_id("mini_chat.list_models")
        .summary("List available AI models")
        .description("Models that are enabled in the current policy catalog.")
        .tag(TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .handler(handlers::list_models)
        .json_response_with_schema::<ModelListDto>(openapi, StatusCode::OK, "List of models")
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{url_prefix}/v1/models/{{id}}"))
        .operation_id("mini_chat.get_model")
        .summary("Get model details")
        .description("One enabled model of the current policy catalog.")
        .tag(TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Model identifier")
        .handler(handlers::get_model)
        .json_response_with_schema::<ModelDto>(openapi, StatusCode::OK, "Model details")
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(service))
}

/// Registers the chat, message and reaction endpoints under `url_prefix`.
///
/// All operations are authenticated and require [`MiniChatLicense`]. The
/// services are made available to the handlers as request extensions.
pub fn register_chat_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    chats: Arc<ChatsService>,
    messages: Arc<MessagesService>,
    reactions: Arc<ReactionsService>,
    url_prefix: &str,
) -> Router {
    let router = OperationBuilder::post(format!("{url_prefix}/v1/chats"))
        .operation_id("mini_chat.create_chat")
        .summary("Create a new chat")
        .description("Creates an untitled or titled chat on an enabled model.")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        // Observe-only zones keyed by IP, as documented in the shipped gateway
        // configuration (`config/quickstart.yaml`).
        .with_throttling(ThrottlingSpec {
            rate_limit_zone: Some(CHAT_RATE_LIMIT_ZONE.to_owned()),
            in_flight_limit_zone: Some(CHAT_IN_FLIGHT_ZONE.to_owned()),
            require_security_context: false,
            dry_run: true,
        })
        .json_request::<CreateChatReq>(openapi, "Chat creation data")
        .handler(chat_handlers::create_chat)
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
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{url_prefix}/v1/chats"))
        .operation_id("mini_chat.list_chats")
        .summary("List chats for the current user")
        .description(
            "Live chats of the caller, most recently active first. Cursor pagination with \
             `limit` (default 20, max 100), `cursor`, `$filter` and `$orderby`.",
        )
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .query_param_typed(
            "limit",
            false,
            "Maximum number of chats to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(chat_handlers::list_chats)
        .json_response_with_schema::<Page<ChatDetailDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of chats",
        )
        .with_odata_filter::<ChatFilterField>()
        .with_odata_orderby::<ChatFilterField>()
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{url_prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.get_chat")
        .summary("Get a chat by ID")
        .description("Chat metadata and message count; messages are listed separately.")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .handler(chat_handlers::get_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Chat found")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(format!("{url_prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.update_chat")
        .summary("Update a chat title")
        .description("Renames the chat; the model and every other field stay unchanged.")
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .json_request::<UpdateChatReq>(openapi, "Chat update data")
        .handler(chat_handlers::update_chat)
        .json_response_with_schema::<ChatDetailDto>(openapi, StatusCode::OK, "Updated chat")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(format!("{url_prefix}/v1/chats/{{id}}"))
        .operation_id("mini_chat.delete_chat")
        .summary("Delete a chat")
        .description(
            "Soft-deletes the chat; provider files and the vector store are removed \
             asynchronously.",
        )
        .tag(CHATS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .handler(chat_handlers::delete_chat)
        .no_content_response(StatusCode::NO_CONTENT, "Chat deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(format!("{url_prefix}/v1/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .description(
            "Messages in chronological order with cursor pagination; each message carries \
             its attachments and the caller's reaction.",
        )
        .tag(MESSAGES_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of messages to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(chat_handlers::list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(
            openapi,
            StatusCode::OK,
            "Paginated list of messages",
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

    let reaction_path = format!("{url_prefix}/v1/chats/{{id}}/messages/{{msg_id}}/reaction");
    let router = OperationBuilder::put(reaction_path.clone())
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .description("Only assistant messages accept reactions; a repeated PUT replaces the value.")
        .tag(REACTIONS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(chat_handlers::put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(reaction_path)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .description("Idempotent: 204 whether or not a reaction existed.")
        .tag(REACTIONS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .handler(chat_handlers::delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router
        .layer(axum::Extension(chats))
        .layer(axum::Extension(messages))
        .layer(axum::Extension(reactions))
}

/// Registers the streaming endpoint and the Turn Status API under `url_prefix`.
///
/// Both operations are authenticated and require [`MiniChatLicense`]. The
/// services are made available to the handlers as request extensions.
pub fn register_stream_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    stream: Arc<StreamService>,
    turns: Arc<TurnsService>,
    url_prefix: &str,
) -> Router {
    let router = OperationBuilder::post(format!("{url_prefix}/v1/chats/{{id}}/messages:stream"))
        .operation_id("mini_chat.stream_message")
        .summary("Send a message and stream the response via SSE")
        .description(
            "Streams the answer as server-sent events: `stream_started`, `ping`, `delta`, \
             `tool`, `citations`, then exactly one `done` or `error`. Failures before the \
             stream opens are normal JSON problems.",
        )
        .tag(MESSAGES_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .json_request::<StreamMessageRequest>(openapi, "Message to send")
        .handler(stream_handlers::stream_message)
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
        .register(router, openapi);

    let router =
        OperationBuilder::get(format!("{url_prefix}/v1/chats/{{id}}/turns/{{request_id}}"))
            .operation_id("mini_chat.get_turn")
            .summary("Get the status of a turn")
            .description(
                "Authoritative turn state backed by `chat_turns`: `running`, `done`, `error` or \
             `cancelled`.",
            )
            .tag(TURNS_TAG)
            .authenticated()
            .require_license_features([MiniChatLicense])
            .path_param("id", "Chat UUID")
            .path_param("request_id", "Turn request UUID")
            .handler(stream_handlers::get_turn)
            .json_response_with_schema::<TurnStatusResponse>(openapi, StatusCode::OK, "Turn found")
            .error_400(openapi)
            .error_401(openapi)
            .error_403(openapi)
            .error_404(openapi)
            .error_500(openapi)
            .error_503(openapi)
            .register(router, openapi);

    router
        .layer(axum::Extension(stream))
        .layer(axum::Extension(turns))
}

/// Registers the turn mutations under `url_prefix`: retry
/// (`POST .../turns/{request_id}/retry`), edit (`PATCH .../turns/{request_id}`)
/// and delete (`DELETE .../turns/{request_id}`) of the latest turn.
///
/// Every operation is authenticated and requires [`MiniChatLicense`]. Retry and
/// edit answer with the SSE contract of `messages:stream`; delete answers 204.
pub fn register_turn_mutation_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    mutations: Arc<TurnMutationService>,
    url_prefix: &str,
) -> Router {
    let turn_path = format!("{url_prefix}/v1/chats/{{id}}/turns/{{request_id}}");

    let router = OperationBuilder::post(format!("{turn_path}/retry"))
        .operation_id("mini_chat.retry_turn")
        .summary("Retry the last turn and stream the new response via SSE")
        .description(
            "Replaces the latest turn by a new turn that answers its original message again. \
             The new `request_id` is announced by `stream_started`. Rejections are normal JSON \
             problems: 400 (turn still running, kill switch, attachments), 404, 409 \
             (`NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`), 429.",
        )
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Request UUID of the turn to retry")
        .handler(stream_handlers::retry_turn)
        .sse_json::<MiniChatSseEvent>(openapi, "SSE stream of chat response events")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::patch(turn_path.clone())
        .operation_id("mini_chat.edit_turn")
        .summary("Edit the last turn and stream the new response via SSE")
        .description(
            "Replaces the latest turn by a new turn with the edited user message. Same stream \
             and error contract as the retry, plus 400 `EMPTY_CONTENT`.",
        )
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Request UUID of the turn to edit")
        .json_request::<EditTurnRequest>(openapi, "New content of the user message")
        .handler(stream_handlers::edit_turn)
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
        .register(router, openapi);

    let router = OperationBuilder::delete(turn_path)
        .operation_id("mini_chat.delete_turn")
        .summary("Delete the last turn")
        .description(
            "Soft-deletes the latest turn and its messages; no turn is created. 204 without a \
             body.",
        )
        .tag(TURNS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("request_id", "Request UUID of the turn to delete")
        .handler(stream_handlers::delete_turn)
        .no_content_response(StatusCode::NO_CONTENT, "Turn deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(mutations))
}

/// Registers `GET {url_prefix}/v1/quota/status`: the authenticated user's
/// quota per tier and period. Authenticated, requires [`MiniChatLicense`].
pub fn register_quota_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    quota: Arc<QuotaStatusService>,
    url_prefix: &str,
) -> Router {
    let router = OperationBuilder::get(format!("{url_prefix}/v1/quota/status"))
        .operation_id("mini_chat.get_quota_status")
        .summary("Get quota status for the authenticated user")
        .description(
            "Per-tier, per-period quota of the caller: limit, used (spent + reserved), \
             remaining, remaining percentage, next reset, warning and exhausted flags.",
        )
        .tag(QUOTAS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .handler(quota_handlers::get_quota_status)
        .json_response_with_schema::<QuotaStatusResponse>(
            openapi,
            StatusCode::OK,
            "Quota status with remaining percentages and warning flags",
        )
        .error_401(openapi)
        .error_403(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(quota))
}

/// Registers the upload operation without the gateway's MIME gate.
///
/// `multipart_file_request` declares the multipart body and, as a side effect,
/// asks the gateway to answer 400 `MISSING_CONTENT_TYPE` itself. The upload must
/// first resolve the chat and the model (404 / `INVALID_MODEL`, "before the body
/// is read") and answer a missing or boundary-less `Content-Type` with
/// `BOUNDARY_REQUIRED`, so the gate is dropped from the registered spec and the
/// handler validates the header.
struct WithoutMimeGate<'a>(&'a dyn OpenApiRegistry);

impl OpenApiRegistry for WithoutMimeGate<'_> {
    fn register_operation(&self, spec: &toolkit::api::operation_builder::OperationSpec) {
        let mut spec = spec.clone();
        spec.allowed_request_content_types = None;
        self.0.register_operation(&spec);
    }

    fn ensure_schema_raw(
        &self,
        name: &str,
        schemas: Vec<(
            String,
            utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
        )>,
    ) -> String {
        self.0.ensure_schema_raw(name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self.0.as_any()
    }
}

/// Registers the attachment endpoints under `url_prefix`: upload
/// (`POST .../attachments`, multipart, 201 with `Location`), get and delete.
///
/// Every operation is authenticated and requires [`MiniChatLicense`]. The
/// service is made available to the handlers as a request extension.
pub fn register_attachment_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    attachments: Arc<AttachmentService>,
    url_prefix: &str,
) -> Router {
    let attachments_path = format!("{url_prefix}/v1/chats/{{id}}/attachments");
    let ungated = WithoutMimeGate(openapi);

    let router = OperationBuilder::post(attachments_path.clone())
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .description(
            "Uploads one file (`multipart/form-data`, field `file`) to the provider and, for a \
             document, indexes it in the chat vector store. 201 with the attachment; a document \
             that is still being indexed after 25 s is returned as `uploaded` and finishes in \
             the background.",
        )
        .tag(ATTACHMENTS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .multipart_file_request("file", Some("File to upload"))
        .handler(attachment_handlers::upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(
            &ungated,
            StatusCode::CREATED,
            "Attachment uploaded and processed",
        )
        .response_header(ResponseHeaderSpec::new(
            "Location",
            "Path of the created attachment",
            ResponseHeaderType::String,
        ))
        .error_400(&ungated)
        .error_401(&ungated)
        .error_403(&ungated)
        .error_404(&ungated)
        .error_409(&ungated)
        .error_429(&ungated)
        .error_500(&ungated)
        .error_503(&ungated)
        .register(router, &ungated);

    let item_path = format!("{attachments_path}/{{attachment_id}}");
    let router = OperationBuilder::get(item_path.clone())
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .description(
            "Status and metadata of an attachment of the caller; poll it after an upload that \
             returned `uploaded`.",
        )
        .tag(ATTACHMENTS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(attachment_handlers::get_attachment)
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
        .register(router, openapi);

    let router = OperationBuilder::delete(item_path)
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .description(
            "Soft-deletes an attachment that no message references (409 `attachment_locked` \
             otherwise); the provider file is removed asynchronously. Idempotent.",
        )
        .tag(ATTACHMENTS_TAG)
        .authenticated()
        .require_license_features([MiniChatLicense])
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .handler(attachment_handlers::delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .register(router, openapi);

    router.layer(axum::Extension(attachments))
}
