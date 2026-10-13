//! `DomainError` to canonical `Problem` mapping (`cpt-cf-mini-chat-algo-gear-foundation-map-error`).
//!
//! One `From<DomainError> for CanonicalError` covers every row of the mapping
//! table; handlers return the domain error and let `?` convert it. Driver,
//! backend, PDP and internal diagnostics are logged here and never reach the
//! `detail` of the response. The canonical envelope carries no top-level `code`.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-error-contract:p1

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{
    DisabledFeature, DomainError, MultipartIssue, QuotaSubject, ResourceKind,
};

/// `Retry-After` of a PDP failure and of the upload concurrency limit.
const RETRY_AFTER_SHORT_SECS: u64 = 5;
/// `Retry-After` of a storage backend failure.
const RETRY_AFTER_STORAGE_SECS: u64 = 10;

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
struct ChatResource;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
struct MessageResource;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
struct TurnResource;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
struct AttachmentResource;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
struct ModelResource;

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-problem
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-lookup-category
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-match-category
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-no-code-field
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-return-problem-object
        match err {
            DomainError::NotFound { resource } => not_found(resource),
            DomainError::InvalidQuery(source) => CanonicalError::from(source),
            DomainError::InvalidModel
            | DomainError::EmptyContent
            | DomainError::InvalidTitle
            | DomainError::InvalidReaction
            | DomainError::UnsupportedContentType
            | DomainError::CodeInterpreterUnavailable
            | DomainError::InvalidMultipart(_)
            | DomainError::CleanupPayloadTooLarge { .. }
            | DomainError::VisionNotSupported
            | DomainError::InvalidAttachmentIds => invalid_argument(&err),
            DomainError::FileTooLarge
            | DomainError::TooManyImages
            | DomainError::InputTooLong
            | DomainError::ContextBudgetExceeded => out_of_range(&err),
            DomainError::FeatureDisabled(_)
            | DomainError::TurnNotTerminal
            | DomainError::ReactionTargetNotAssistant => failed_precondition(&err),
            DomainError::AuthzDenied => permission_denied(),
            DomainError::PdpUnavailable | DomainError::UploadConcurrencyLimit => {
                service_unavailable(RETRY_AFTER_SHORT_SECS)
            }
            DomainError::StorageUnavailable => service_unavailable(RETRY_AFTER_STORAGE_SECS),
            DomainError::TurnAlreadyRunning
            | DomainError::RequestIdConflict
            | DomainError::NotLatestTurn
            | DomainError::GenerationInProgress
            | DomainError::Replay => aborted(&err),
            DomainError::AttachmentLocked
            | DomainError::ProviderMismatch
            | DomainError::UniqueViolation => already_exists(&err),
            DomainError::QuotaExceeded(_)
            | DomainError::DocumentLimit
            | DomainError::StorageLimit => resource_exhausted(&err),
            DomainError::ProviderResolution(_)
            | DomainError::PolicyResolution(_)
            | DomainError::Database(_)
            | DomainError::Internal(_) => internal(&err),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-return-problem-object
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-no-code-field
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-match-category
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-lookup-category
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-return-problem
    }
}

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-not-found
fn not_found(resource: ResourceKind) -> CanonicalError {
    match resource {
        ResourceKind::Chat => ChatResource::not_found("Chat not found")
            .with_resource("chat")
            .create(),
        ResourceKind::Message => MessageResource::not_found("Message not found")
            .with_resource("message")
            .create(),
        ResourceKind::Turn => TurnResource::not_found("Turn not found")
            .with_resource("turn")
            .create(),
        ResourceKind::Attachment => AttachmentResource::not_found("Attachment not found")
            .with_resource("attachment")
            .create(),
        ResourceKind::Model => ModelResource::not_found("Model not found")
            .with_resource("model")
            .create(),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-not-found

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-field-violations
// @cpt-dod:cpt-cf-mini-chat-dod-attachments-error-contract:p1
fn invalid_argument(err: &DomainError) -> CanonicalError {
    match err {
        DomainError::InvalidModel => ModelResource::invalid_argument()
            .with_field_violation("model", "The model is invalid or disabled", "INVALID_MODEL")
            .create(),
        DomainError::EmptyContent => MessageResource::invalid_argument()
            .with_field_violation("content", "The content must not be empty", "EMPTY_CONTENT")
            .create(),
        DomainError::InvalidTitle => ChatResource::invalid_argument()
            .with_field_violation("title", "The title is invalid", "INVALID_TITLE")
            .create(),
        DomainError::InvalidReaction => MessageResource::invalid_argument()
            .with_field_violation(
                "reaction",
                "The reaction must be `like` or `dislike`",
                "INVALID_REACTION",
            )
            .create(),
        DomainError::UnsupportedContentType => AttachmentResource::invalid_argument()
            .with_field_violation(
                "file",
                "The content type is not supported",
                "UNSUPPORTED_CONTENT_TYPE",
            )
            .create(),
        DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
            .with_field_violation(
                "file",
                "The file can only be used with the code interpreter, which is unavailable",
                "CODE_INTERPRETER_UNAVAILABLE",
            )
            .create(),
        DomainError::InvalidMultipart(issue) => {
            let (field, description, reason) = match issue {
                MultipartIssue::BoundaryRequired => (
                    "content_type",
                    "The multipart boundary is required",
                    "BOUNDARY_REQUIRED",
                ),
                MultipartIssue::MultipartError => (
                    "multipart",
                    "The multipart body is malformed",
                    "MULTIPART_ERROR",
                ),
                MultipartIssue::MissingFile => {
                    ("file", "The `file` part is required", "MISSING_FILE")
                }
                MultipartIssue::MissingContentType => (
                    "content_type",
                    "The `file` part has no content type",
                    "MISSING_CONTENT_TYPE",
                ),
            };
            AttachmentResource::invalid_argument()
                .with_field_violation(field, description, reason)
                .create()
        }
        DomainError::CleanupPayloadTooLarge { detail } => ChatResource::invalid_argument()
            .with_format(detail.clone())
            .create(),
        DomainError::VisionNotSupported => MessageResource::invalid_argument()
            .with_field_violation(
                "attachment",
                "The model does not support image input",
                "VISION_NOT_SUPPORTED",
            )
            .create(),
        _ => MessageResource::invalid_argument()
            .with_field_violation(
                "attachment",
                "One or more attachments are invalid",
                "invalid_attachment",
            )
            .create(),
    }
}

fn out_of_range(err: &DomainError) -> CanonicalError {
    match err {
        DomainError::FileTooLarge => AttachmentResource::out_of_range("Value out of range")
            .with_field_violation(
                "content_length",
                "The file exceeds the maximum size",
                "FILE_TOO_LARGE",
            )
            .create(),
        DomainError::TooManyImages => MessageResource::out_of_range("Value out of range")
            .with_field_violation(
                "image_count",
                "The message carries too many images",
                "TOO_MANY_IMAGES",
            )
            .create(),
        DomainError::InputTooLong => MessageResource::out_of_range("Value out of range")
            .with_field_violation("content", "The input is too long", "INPUT_TOO_LONG")
            .create(),
        _ => MessageResource::out_of_range("Value out of range")
            .with_field_violation(
                "content",
                "The context budget is exceeded",
                "CONTEXT_BUDGET_EXCEEDED",
            )
            .create(),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-field-violations

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-precondition
fn failed_precondition(err: &DomainError) -> CanonicalError {
    match err {
        DomainError::FeatureDisabled(feature) => {
            let subject = match feature {
                DisabledFeature::WebSearch => "web_search",
                DisabledFeature::Images => "images",
            };
            ChatResource::failed_precondition()
                .with_precondition_violation(subject, "The feature is disabled", "FEATURE_DISABLED")
                .create()
        }
        DomainError::TurnNotTerminal => TurnResource::failed_precondition()
            .with_precondition_violation(
                "turn_state",
                "The turn is not in a terminal state",
                "STATE",
            )
            .create(),
        _ => MessageResource::failed_precondition()
            .with_precondition_violation(
                "reaction_target",
                "Reactions apply to assistant messages only",
                "STATE",
            )
            .create(),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-precondition

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-exhausted
fn resource_exhausted(err: &DomainError) -> CanonicalError {
    let (subject, description) = match err {
        DomainError::QuotaExceeded(QuotaSubject::Tokens) => ("tokens", "quota_exceeded"),
        DomainError::QuotaExceeded(QuotaSubject::WebSearch) => ("web_search", "quota_exceeded"),
        DomainError::QuotaExceeded(QuotaSubject::CodeInterpreter) => {
            ("code_interpreter", "quota_exceeded")
        }
        DomainError::DocumentLimit => ("document_limit", "document_limit"),
        _ => ("storage_limit", "storage_limit"),
    };
    ChatResource::resource_exhausted("Quota or limit exceeded")
        .with_quota_violation(subject, description)
        .create()
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-exhausted

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-reason
fn aborted(err: &DomainError) -> CanonicalError {
    let reason = match err {
        DomainError::TurnAlreadyRunning => "turn_already_running",
        DomainError::RequestIdConflict => "request_id_conflict",
        DomainError::NotLatestTurn => "NOT_LATEST_TURN",
        DomainError::GenerationInProgress => "GENERATION_IN_PROGRESS",
        _ => "REPLAY",
    };
    TurnResource::aborted("The request conflicts with the state of the turn")
        .with_reason(reason)
        .create()
}

fn permission_denied() -> CanonicalError {
    ChatResource::permission_denied()
        .with_reason("AUTHZ_DENIED")
        .create()
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-reason

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-already-exists
fn already_exists(err: &DomainError) -> CanonicalError {
    let (code, resource) = match err {
        DomainError::AttachmentLocked => ("attachment_locked", ResourceKind::Attachment),
        DomainError::ProviderMismatch => ("provider_mismatch", ResourceKind::Attachment),
        _ => ("unique_violation", ResourceKind::Chat),
    };
    match resource {
        ResourceKind::Attachment => AttachmentResource::already_exists("The resource conflicts")
            .with_resource(code)
            .create(),
        _ => ChatResource::already_exists("The resource conflicts")
            .with_resource(code)
            .create(),
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-already-exists

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-unavailable
fn service_unavailable(retry_after_seconds: u64) -> CanonicalError {
    CanonicalError::service_unavailable()
        .with_retry_after_seconds(retry_after_seconds)
        .create()
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-unavailable

// @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-internal
fn internal(err: &DomainError) -> CanonicalError {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-sanitize-detail
    // The cause can carry driver text, provider identifiers or secrets: it is
    // logged and replaced by a generic diagnostic on the way out.
    tracing::error!(error = %err, "internal error mapped to a generic problem");
    CanonicalError::internal("internal error").create()
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-sanitize-detail
}
// @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-map-error:p1:inst-case-internal

#[cfg(test)]
#[path = "error_mapping_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "error_mapping_attachments_tests.rs"]
mod attachments_tests;
