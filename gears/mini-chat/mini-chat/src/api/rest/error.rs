//! `DomainError` → canonical `Problem` mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::DomainError;

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

/// Retry-After of authorization (PDP) failures and the upload concurrency limit.
pub const RETRY_AFTER_SHORT: u64 = 5;
/// Retry-After of storage backend failures.
pub const RETRY_AFTER_STORAGE: u64 = 10;

/// Maps a domain error to the canonical error of the REST surface.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn to_canonical(err: DomainError) -> CanonicalError {
    match err {
        DomainError::ChatNotFound => ChatResource::not_found("Chat not found").with_resource("chat").create(),
        DomainError::MessageNotFound => MessageResource::not_found("Message not found")
            .with_resource("message")
            .create(),
        DomainError::TurnNotFound => TurnResource::not_found("Turn not found").with_resource("turn").create(),
        DomainError::AttachmentNotFound => AttachmentResource::not_found("Attachment not found")
            .with_resource("attachment")
            .create(),
        DomainError::ModelNotFound => ModelResource::not_found("Model not found").with_resource("model").create(),

        DomainError::InvalidModel => ChatResource::invalid_argument()
            .with_field_violation("model", "Unknown or disabled model", "INVALID_MODEL")
            .create(),
        DomainError::InvalidTitle => ChatResource::invalid_argument()
            .with_field_violation(
                "title",
                "Title must be 1-255 characters and not whitespace-only",
                "INVALID_TITLE",
            )
            .create(),
        DomainError::EmptyContent => MessageResource::invalid_argument()
            .with_field_violation("content", "Content must not be empty", "EMPTY_CONTENT")
            .create(),
        DomainError::InvalidReaction => MessageResource::invalid_argument()
            .with_field_violation("reaction", "Reaction must be 'like' or 'dislike'", "INVALID_REACTION")
            .create(),
        DomainError::InvalidAttachment(detail) => AttachmentResource::invalid_argument()
            .with_field_violation("attachment", detail, "invalid_attachment")
            .create(),
        DomainError::TooManyImages => MessageResource::out_of_range("Too many images in one message")
            .with_field_violation("image_count", "Too many images in one message", "TOO_MANY_IMAGES")
            .create(),
        DomainError::VisionNotSupported => MessageResource::invalid_argument()
            .with_field_violation(
                "content_type",
                "The model does not support image input",
                "VISION_NOT_SUPPORTED",
            )
            .create(),
        DomainError::InputTooLong => MessageResource::out_of_range("Message exceeds the model input limit")
            .with_field_violation("content", "Message exceeds the model input limit", "INPUT_TOO_LONG")
            .create(),
        DomainError::ContextBudgetExceeded => MessageResource::out_of_range("Mandatory context does not fit the token budget")
            .with_field_violation(
                "content",
                "Mandatory context does not fit the token budget",
                "CONTEXT_BUDGET_EXCEEDED",
            )
            .create(),
        DomainError::FeatureDisabled(subject) => ChatResource::failed_precondition()
            .with_precondition_violation(subject.as_str(), "Feature is disabled", "FEATURE_DISABLED")
            .create(),
        DomainError::TurnNotTerminal => TurnResource::failed_precondition()
            .with_precondition_violation("turn_state", "The turn is not in a terminal state", "STATE")
            .create(),
        DomainError::ReactionTargetNotAssistant => MessageResource::failed_precondition()
            .with_precondition_violation(
                "reaction_target",
                "Reactions are allowed on assistant messages only",
                "STATE",
            )
            .create(),

        DomainError::FileTooLarge => AttachmentResource::out_of_range("File too large")
            .with_field_violation("content_length", "File exceeds the size limit", "FILE_TOO_LARGE")
            .create(),
        DomainError::UnsupportedContentType => AttachmentResource::invalid_argument()
            .with_field_violation("content_type", "Unsupported file type", "UNSUPPORTED_CONTENT_TYPE")
            .create(),
        DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
            .with_field_violation(
                "file",
                "Code interpreter is unavailable for this file type",
                "CODE_INTERPRETER_UNAVAILABLE",
            )
            .create(),
        DomainError::Multipart { field, reason, detail } => AttachmentResource::invalid_argument()
            .with_field_violation(field, detail, reason)
            .create(),

        DomainError::AccessDenied => ChatResource::permission_denied().with_reason("AUTHZ_DENIED").create(),
        DomainError::AuthzUnavailable => CanonicalError::service_unavailable()
            .with_detail("Authorization service is temporarily unavailable")
            .with_retry_after_seconds(RETRY_AFTER_SHORT)
            .create(),

        DomainError::TurnAlreadyRunning => ChatResource::aborted("Another turn is running in this chat")
            .with_reason("turn_already_running")
            .create(),
        DomainError::RequestIdConflict => TurnResource::aborted("The request id is already used by another turn")
            .with_reason("request_id_conflict")
            .create(),
        DomainError::NotLatestTurn => TurnResource::aborted("Only the latest turn can be modified")
            .with_reason("NOT_LATEST_TURN")
            .create(),
        DomainError::GenerationInProgress => TurnResource::aborted("A generation is already in progress")
            .with_reason("GENERATION_IN_PROGRESS")
            .create(),
        DomainError::Replay => TurnResource::aborted("The turn is already completed")
            .with_reason("REPLAY")
            .create(),
        DomainError::AttachmentLocked => AttachmentResource::already_exists("Attachment is referenced by a message")
            .with_resource("attachment_locked")
            .create(),
        DomainError::ProviderMismatch => AttachmentResource::already_exists(
            "The chat's vector store belongs to another provider backend",
        )
        .with_resource("provider_mismatch")
        .create(),
        DomainError::UniqueViolation => ChatResource::already_exists("Conflicting resource state")
            .with_resource("unique_violation")
            .create(),

        DomainError::QuotaExceeded(scope) => ChatResource::resource_exhausted("Quota exceeded")
            .with_quota_violation(scope.as_str(), "quota_exceeded")
            .create(),
        DomainError::DocumentLimit => AttachmentResource::resource_exhausted("Per-chat document limit reached")
            .with_quota_violation("document_limit", "Per-chat document limit reached")
            .create(),
        DomainError::StorageLimit => AttachmentResource::resource_exhausted("Per-chat storage limit reached")
            .with_quota_violation("storage_limit", "Per-chat storage limit reached")
            .create(),

        DomainError::StorageUnavailable(cause) => {
            tracing::warn!(%cause, "storage backend unavailable");
            CanonicalError::service_unavailable()
                .with_detail("File storage is temporarily unavailable")
                .with_retry_after_seconds(RETRY_AFTER_STORAGE)
                .create()
        }
        DomainError::UploadConcurrency => CanonicalError::service_unavailable()
            .with_detail("Too many concurrent uploads")
            .with_retry_after_seconds(RETRY_AFTER_SHORT)
            .create(),

        DomainError::OData(e) => CanonicalError::from(e),
        DomainError::OutboxPayloadTooLarge(detail) => ChatResource::invalid_argument().with_format(detail).create(),
        DomainError::Internal(diagnostic) => {
            tracing::error!(%diagnostic, "internal error");
            CanonicalError::internal(diagnostic).create()
        }
    }
}

/// Like [`to_canonical`], but an outbox payload-size failure is a 500
/// (attachment `DELETE`, turn retry/edit/delete).
#[must_use]
pub fn to_canonical_strict(err: DomainError) -> CanonicalError {
    match err {
        DomainError::OutboxPayloadTooLarge(d) => to_canonical(DomainError::Internal(d)),
        other => to_canonical(other),
    }
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        to_canonical(err)
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
