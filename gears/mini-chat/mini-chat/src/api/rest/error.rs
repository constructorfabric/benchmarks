//! Domain error to canonical error mapping, and the SSE `error` envelope.
//!
//! The per-condition table is DESIGN 3.3 `Error Codes` and ADR-0004. A REST error is a
//! canonical Problem with `type`, `title`, `status`, `detail`, `instance`, `trace_id`
//! and `context` and no top-level `code`; the `{code, message}` shape is reserved for
//! the SSE `error` event.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{
    AttachmentError, ConflictReason, DomainError, EnqueueSite, QuotaScope, ResourceKind,
    StreamErrorCode,
};
use crate::domain::streaming::provider_errors::sanitize_provider_text;

/// `Retry-After` of a PDP failure (seconds).
const PDP_RETRY_AFTER_SECONDS: u64 = 5;

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-build-problem
#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub(crate) struct ChatResource;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub(crate) struct MessageResource;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub(crate) struct TurnResource;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub(crate) struct AttachmentResource;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub(crate) struct ModelResource;
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-build-problem

/// `not_found` carrying the missing resource type in `context.resource_type`.
fn not_found(resource: ResourceKind, id: Option<String>) -> CanonicalError {
    // the echoed name is the requested identifier; without one, the resource kind
    let name = |kind: &str| id.unwrap_or_else(|| kind.to_owned());
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-not-found
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-resource-type
    match resource {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-chat-404
        // a missing, soft-deleted, foreign-user or foreign-tenant chat is never a 403
        ResourceKind::Chat => ChatResource::not_found("chat not found")
            .with_resource(name("chat"))
            .create(),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-chat-404
        ResourceKind::Message => MessageResource::not_found("message not found")
            .with_resource(name("message"))
            .create(),
        ResourceKind::Turn => TurnResource::not_found("turn not found")
            .with_resource(name("turn"))
            .create(),
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-attachment-404
        // unknown, soft-deleted, other-chat and other-uploader attachments are the same 404
        ResourceKind::Attachment => AttachmentResource::not_found("attachment not found")
            .with_resource(name("attachment"))
            .create(),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-attachment-404
        ResourceKind::Model => ModelResource::not_found("model not found")
            .with_resource(name("model"))
            .create(),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-resource-type
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-not-found
}

/// An oversized outbox payload: 400 `invalid_argument` on chat deletion (the message is also
/// carried in `context.format`), 500 `internal` on every other call site.
fn payload_too_large(site: EnqueueSite) -> CanonicalError {
    match site {
        EnqueueSite::ChatDelete => ChatResource::invalid_argument()
            .with_format("the chat cleanup request is too large to be queued")
            .create(),
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-payload-500
        EnqueueSite::AttachmentDelete
        | EnqueueSite::TurnRetry
        | EnqueueSite::TurnEdit
        | EnqueueSite::TurnDelete
        | EnqueueSite::TurnFinalize => {
            tracing::error!(?site, "outbox payload exceeds the size limit");
            CanonicalError::internal("outbox payload exceeds the size limit").create()
        } // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit:p1:inst-mc-return-payload-500
    }
}

/// 403 `permission_denied` with the `AUTHZ_DENIED` reason; the deny details are never exposed.
fn authz_denied() -> CanonicalError {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-permission-denied
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-reason-location
    ChatResource::permission_denied()
        .with_reason("AUTHZ_DENIED")
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-reason-location
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-permission-denied
}

/// 503 `service_unavailable` with `Retry-After: 5`; the cause stays in the log.
fn authz_unavailable(cause: &str) -> CanonicalError {
    tracing::error!(cause = %cause, "authorization evaluation failed");
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-unavailable
    CanonicalError::service_unavailable()
        .with_retry_after_seconds(PDP_RETRY_AFTER_SECONDS)
        .with_detail("authorization is temporarily unavailable")
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-unavailable
}

/// 500 `internal` with a generic detail; the diagnostic stays in the log.
fn internal(diagnostic: String) -> CanonicalError {
    tracing::error!(diagnostic = %diagnostic, "internal error");
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-internal
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-internal
    CanonicalError::internal(diagnostic).create()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-internal
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-internal
}

/// 429 `resource_exhausted` with one `context.violations[]` entry
/// `{subject: <quota scope>, description: "quota_exceeded"}`; clients branch on the subject.
fn quota_exceeded(scope: QuotaScope) -> CanonicalError {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-quota
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-return-quota
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-before-stream
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-not-implemented-scopes
    // a JSON Problem before any SSE stream opens; the scope is one of the three implemented
    // ones, never an image scope (the enum has no such variant)
    TurnResource::resource_exhausted("the quota is exhausted")
        .with_quota_violation(scope.as_str(), "quota_exceeded")
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-not-implemented-scopes
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-before-stream
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-return-quota
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-quota
}

/// 400 `failed_precondition` with `violations[{subject, type: FEATURE_DISABLED}]`.
fn feature_disabled(subject: &'static str) -> CanonicalError {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-kill
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-return-kill
    TurnResource::failed_precondition()
        .with_precondition_violation(subject, "the feature is disabled", "FEATURE_DISABLED")
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-return-kill
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-kill
}

/// 409 `already_exists` carrying the conflict code in `context.resource_name`.
fn unique_violation(code: String) -> CanonicalError {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-conflict
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-unique-violation
    ChatResource::already_exists("the resource already exists")
        .with_resource(code)
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-unique-violation
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-conflict
}

/// 400 `invalid_argument` with `field_violations[model].reason = INVALID_MODEL`.
fn invalid_model(model: &str) -> CanonicalError {
    tracing::debug!(model = %model, "rejected an unknown or disabled model");
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-invalid-argument
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-invalid-model
    ChatResource::invalid_argument()
        .with_field_violation("model", "unknown or disabled model", "INVALID_MODEL")
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-invalid-model
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-case-invalid-argument
}

/// 400 `invalid_argument` with `field_violations[title].reason = INVALID_TITLE`.
fn invalid_title() -> CanonicalError {
    ChatResource::invalid_argument()
        .with_field_violation(
            "title",
            "the title must have 1 to 255 characters after trimming",
            "INVALID_TITLE",
        )
        .create()
}

// @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-content
/// 400 `invalid_argument` with `field_violations[content].reason = EMPTY_CONTENT`.
fn empty_content() -> CanonicalError {
    MessageResource::invalid_argument()
        .with_field_violation("content", "the content must not be empty", "EMPTY_CONTENT")
        .create()
}

/// 400 `invalid_argument` with `field_violations[attachment].reason = invalid_attachment`.
fn invalid_attachment() -> CanonicalError {
    MessageResource::invalid_argument()
        .with_field_violation(
            "attachment",
            "an attachment is invalid, duplicated, not ready or not available in this chat",
            "invalid_attachment",
        )
        .create()
}

/// 400 `out_of_range` with `field_violations[image_count].reason = TOO_MANY_IMAGES`.
fn too_many_images() -> CanonicalError {
    MessageResource::out_of_range("too many images")
        .with_field_violation(
            "image_count",
            "the message references too many images",
            "TOO_MANY_IMAGES",
        )
        .create()
}
// @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-content

// @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-model
/// 400 `invalid_argument` with `field_violations[content_type].reason = VISION_NOT_SUPPORTED`.
fn vision_not_supported() -> CanonicalError {
    MessageResource::invalid_argument()
        .with_field_violation(
            "content_type",
            "the model does not support image input",
            "VISION_NOT_SUPPORTED",
        )
        .create()
}
// @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-model

// @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-budget
/// 400 `out_of_range` with `field_violations[content].reason = INPUT_TOO_LONG`.
fn input_too_long() -> CanonicalError {
    MessageResource::out_of_range("the message is too long")
        .with_field_violation(
            "content",
            "the message exceeds the input limit of the model",
            "INPUT_TOO_LONG",
        )
        .create()
}

/// 400 `out_of_range` with `field_violations[context].reason = CONTEXT_BUDGET_EXCEEDED`.
fn context_budget_exceeded() -> CanonicalError {
    MessageResource::out_of_range("the context does not fit the budget")
        .with_field_violation(
            "context",
            "the mandatory context does not fit the context budget of the model",
            "CONTEXT_BUDGET_EXCEEDED",
        )
        .create()
}
// @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-budget

/// 409 `aborted` with `context.reason` `request_id_conflict` or `turn_already_running`; the
/// `detail` is generic text and the internal cause (turn ids, driver text) is only logged.
fn conflict(reason: ConflictReason) -> CanonicalError {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-409
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-detail
    // the `detail` is human-readable text outside the contract: clients branch on the status,
    // the category and `context.reason`
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-409
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-detail
    TurnResource::aborted("the request conflicts with the state of the chat")
        .with_reason(reason.as_str())
        .create()
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-detail
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-409
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-detail
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-409
}

/// The attachment error contract of the upload route (`cpt-cf-mini-chat-algo-attachments-and-
/// file-search-error-contract`): 400 `invalid_argument` with the reason of the failed intake step,
/// 400 `out_of_range` `FILE_TOO_LARGE`, 429 `resource_exhausted` with the limit subject, 409
/// `already_exists` `provider_mismatch`, and 503 `service_unavailable` with `Retry-After` 10
/// (provider failure) or 5 (concurrency limit). No provider identifier, provider text or row
/// `error_code` reaches the Problem.
fn attachment_error(error: AttachmentError) -> CanonicalError {
    let invalid = |field: &str, reason: &str| {
        AttachmentResource::invalid_argument()
            .with_field_violation(field, "the upload request is invalid", reason)
            .create()
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-match
    match error {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-multipart
        AttachmentError::BoundaryRequired => invalid("content_type", "BOUNDARY_REQUIRED"),
        AttachmentError::MultipartError => invalid("multipart", "MULTIPART_ERROR"),
        AttachmentError::MissingFile => invalid("file", "MISSING_FILE"),
        AttachmentError::MissingContentType => invalid("content_type", "MISSING_CONTENT_TYPE"),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-multipart
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-unsupported
        AttachmentError::UnsupportedContentType => {
            invalid("content_type", "UNSUPPORTED_CONTENT_TYPE")
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-unsupported
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-ci
        AttachmentError::CodeInterpreterUnavailable => {
            invalid("file", "CODE_INTERPRETER_UNAVAILABLE")
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-ci
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-too-large
        // a body above the api-gateway body limit gets its 413 from the gateway
        AttachmentError::FileTooLarge => AttachmentResource::out_of_range("the file is too large")
            .with_field_violation("content_length", "the file is too large", "FILE_TOO_LARGE")
            .create(),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-too-large
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-limits
        AttachmentError::DocumentLimit => {
            AttachmentResource::resource_exhausted("the document limit is reached")
                .with_quota_violation("document_limit", "quota_exceeded")
                .create()
        }
        AttachmentError::StorageLimit => {
            AttachmentResource::resource_exhausted("the storage limit is reached")
                .with_quota_violation("storage_limit", "quota_exceeded")
                .create()
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-limits
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-mismatch
        AttachmentError::ProviderMismatch => {
            AttachmentResource::already_exists("the storage provider differs")
                .with_resource("provider_mismatch")
                .create()
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-mismatch
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-storage-503
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-no-provider-text
        // the detail is generic: neither the row `error_code` nor provider text reaches it
        AttachmentError::StorageUnavailable => CanonicalError::service_unavailable()
            .with_retry_after_seconds(10)
            .with_detail("Service temporarily unavailable")
            .create(),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-no-provider-text
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-storage-503
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-busy-503
        AttachmentError::UploadBusy => CanonicalError::service_unavailable()
            .with_retry_after_seconds(5)
            .with_detail("Service temporarily unavailable")
            .create(),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-busy-503
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-locked
        AttachmentError::Locked => {
            AttachmentResource::already_exists("the attachment is referenced by a message")
                .with_resource("attachment_locked")
                .create()
        } // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-locked
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-match
}

/// A list query error of the platform `OData` library: 400 `invalid_argument` on the `OData`
/// query resource type with the library reason (`INVALID_FILTER`, `ORDER_MISMATCH`, ...), or a
/// generic 500 for a database failure.
fn odata_query_error(error: toolkit_odata::Error) -> CanonicalError {
    CanonicalError::from(error)
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-match-category
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-match
        let problem = match err {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-404
            // a chat, turn or message that is not visible: 404 with the type of the resource
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-404
            DomainError::NotFound { resource, id } => not_found(resource, id),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-404
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-404
            DomainError::InvalidModel { model } => invalid_model(&model),
            DomainError::InvalidTitle => invalid_title(),
            DomainError::ODataQuery(error) => odata_query_error(error),
            // an audit payload above the outbox limit of a mutation is a 500 (not the 400 of chat
            // deletion), see `payload_too_large`
            DomainError::OutboxPayloadTooLarge { site } => payload_too_large(site),
            // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-rejected
            // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-return-rejection
            // the Problem of a preflight or reserve rejection (400, 429 or 500); no reserve is
            // taken for any of them
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-429
            DomainError::QuotaExceeded { scope } => quota_exceeded(scope),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-429
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-images-disabled
            DomainError::FeatureDisabled { subject } => feature_disabled(subject),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-images-disabled
            // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-return-rejection
            // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-admission:p1:inst-admit-rejected
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-authz
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-authz
            DomainError::AuthzDenied => authz_denied(),
            DomainError::AuthzUnavailable { cause } => authz_unavailable(&cause),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-authz
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-authz
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-internal
            DomainError::PolicyUnavailable { cause } => internal(cause),
            // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping:p1:inst-reject-internal
            DomainError::UniqueViolation { code } => unique_violation(code),
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-internal
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-500
            // a provider or policy resolution failure before streaming, an internal or database
            // error: 500 with a generic detail
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-500
            DomainError::Internal { diagnostic } => internal(diagnostic),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-500
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-500
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-internal
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-match
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-values
            DomainError::EmptyContent => empty_content(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "the reaction must be `like` or `dislike`",
                    "INVALID_REACTION",
                )
                .create(),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-values
            DomainError::InvalidAttachment => invalid_attachment(),
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-preflight
            // with `INVALID_MODEL`, `FEATURE_DISABLED` and the 429 rows above: the retry and edit
            // preflight rejections are the send preflight rows unchanged
            DomainError::TooManyImages => too_many_images(),
            DomainError::VisionNotSupported => vision_not_supported(),
            DomainError::InputTooLong => input_too_long(),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-preflight
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-setup
            DomainError::ContextBudgetExceeded => context_budget_exceeded(),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-setup
            DomainError::Conflict { reason } => conflict(reason),
            // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-state
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation("turn_state", "the turn is not terminal", "STATE")
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "only assistant messages accept a reaction",
                    "STATE",
                )
                .create(),
            // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-state
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-match
            DomainError::Attachment(error) => attachment_error(error),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-match-category
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-match
        tracing::debug!(status = problem.status_code(), "mapped a domain error");
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-return-problem
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-return
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-return
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-return
        // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-json-only
        // every rejection is a JSON Problem: no stream is opened before the setup succeeded
        problem
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-json-only
        // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-error-contract:p1:inst-ec-return
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract:p1:inst-err-return
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-send-error-contract:p1:inst-sec-return
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-return-problem
    }
}

/// The `data` of an SSE `event: error`: an object with only `code` and `message`.
/// The message is the text of the provider-error sanitization (already sanitized, or a generic
/// message), the same text stored as `chat_turns.error_detail`; [`sanitize_provider_text`] is
/// idempotent, so it is applied again only as a guard.
#[must_use]
pub fn sse_error_data(code: StreamErrorCode, message: &str) -> serde_json::Value {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-stream-open
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-sse-event
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-return-sse
    serde_json::json!({
        "code": code.as_str(),
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-detail-policy
        "message": sanitize_provider_text(message),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-detail-policy
    })
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-return-sse
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-sse-event
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-error-mapping:p1:inst-error-stream-open
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
