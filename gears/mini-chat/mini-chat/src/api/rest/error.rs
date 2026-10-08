//! `DomainError` → canonical `Problem` mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, NotFoundKind};

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

fn invalid(field: &str, reason: &str, detail: impl Into<String>) -> CanonicalError {
    ChatResource::invalid_argument()
        .with_field_violation(field, detail, reason)
        .create()
}

fn aborted(reason: &str, detail: &str) -> CanonicalError {
    ChatResource::aborted(detail).with_reason(reason).create()
}

fn unavailable(detail: &str, retry_after: u64) -> CanonicalError {
    CanonicalError::service_unavailable()
        .with_detail(detail)
        .with_retry_after_seconds(retry_after)
        .create()
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines)]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound(kind) => match kind {
                NotFoundKind::Chat => ChatResource::not_found("Chat not found").with_resource("chat").create(),
                NotFoundKind::Message => MessageResource::not_found("Message not found")
                    .with_resource("message")
                    .create(),
                NotFoundKind::Turn => TurnResource::not_found("Turn not found").with_resource("turn").create(),
                NotFoundKind::Attachment => AttachmentResource::not_found("Attachment not found")
                    .with_resource("attachment")
                    .create(),
                NotFoundKind::Model => ModelResource::not_found("Model not found").with_resource("model").create(),
            },
            DomainError::InvalidModel => invalid("model", "INVALID_MODEL", "Unknown or disabled model"),
            DomainError::EmptyContent => invalid("content", "EMPTY_CONTENT", "Content must not be empty"),
            DomainError::InvalidTitle => invalid(
                "title",
                "INVALID_TITLE",
                "Title must be 1-255 characters after trimming",
            ),
            DomainError::InvalidReaction => invalid(
                "reaction",
                "INVALID_REACTION",
                "Reaction must be 'like' or 'dislike'",
            ),
            DomainError::InvalidAttachment(d) => invalid("attachment", "invalid_attachment", d),
            DomainError::VisionNotSupported => invalid(
                "content_type",
                "VISION_NOT_SUPPORTED",
                "The model does not support image input",
            ),
            DomainError::UnsupportedContentType(t) => invalid(
                "content_type",
                "UNSUPPORTED_CONTENT_TYPE",
                format!("Unsupported content type: {t}"),
            ),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "Code interpreter is not available for this file",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart(field, reason, detail) => invalid(field, reason, detail),
            DomainError::PayloadTooLarge(d) => ChatResource::invalid_argument().with_format(d).create(),
            DomainError::FileTooLarge => ChatResource::out_of_range("File too large")
                .with_field_violation("content_length", "File exceeds the size limit", "FILE_TOO_LARGE")
                .create(),
            DomainError::TooManyImages => ChatResource::out_of_range("Too many images")
                .with_field_violation("image_count", "Too many images in one message", "TOO_MANY_IMAGES")
                .create(),
            DomainError::InputTooLong => ChatResource::out_of_range("Message too long")
                .with_field_violation("content", "Message exceeds the input token limit", "INPUT_TOO_LONG")
                .create(),
            DomainError::ContextBudgetExceeded => ChatResource::out_of_range("Context budget exceeded")
                .with_field_violation(
                    "content",
                    "Mandatory context does not fit the token budget",
                    "CONTEXT_BUDGET_EXCEEDED",
                )
                .create(),
            DomainError::FeatureDisabled(subject) => ChatResource::failed_precondition()
                .with_precondition_violation(subject, format!("{subject} is disabled"), "FEATURE_DISABLED")
                .create(),
            DomainError::TurnNotTerminal => ChatResource::failed_precondition()
                .with_precondition_violation("turn_state", "The turn is still running", "STATE")
                .create(),
            DomainError::ReactionTarget => ChatResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "Reactions are allowed on assistant messages only",
                    "STATE",
                )
                .create(),
            DomainError::AccessDenied => ChatResource::permission_denied().with_reason("AUTHZ_DENIED").create(),
            DomainError::AuthzUnavailable(cause) => {
                tracing::error!(cause = %cause, "authorization unavailable");
                unavailable("Service temporarily unavailable", 5)
            }
            DomainError::TurnAlreadyRunning => aborted(
                "turn_already_running",
                "A generation is already running for this chat",
            ),
            DomainError::RequestIdConflict => aborted("request_id_conflict", "The request_id is already in use"),
            DomainError::NotLatestTurn => aborted("NOT_LATEST_TURN", "Only the latest turn can be changed"),
            DomainError::GenerationInProgress => aborted("GENERATION_IN_PROGRESS", "A generation is in progress"),
            DomainError::Replay => aborted("REPLAY", "The turn was already completed"),
            DomainError::AttachmentLocked => AttachmentResource::already_exists("Attachment is referenced by a message")
                .with_resource("attachment_locked")
                .create(),
            DomainError::ProviderMismatch => ChatResource::already_exists(
                "The chat vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation => ChatResource::already_exists("Resource already exists")
                .with_resource("unique_violation")
                .create(),
            DomainError::QuotaExceeded(scope) => ChatResource::resource_exhausted("Quota exceeded")
                .with_quota_violation(scope, "quota_exceeded")
                .create(),
            DomainError::DocumentLimit => ChatResource::resource_exhausted("Document limit reached")
                .with_quota_violation("document_limit", "Per-chat document limit reached")
                .create(),
            DomainError::StorageLimit => ChatResource::resource_exhausted("Storage limit reached")
                .with_quota_violation("storage_limit", "Per-chat storage limit reached")
                .create(),
            DomainError::StorageUnavailable(cause) => {
                tracing::warn!(cause = %cause, "storage backend unavailable");
                unavailable("Service temporarily unavailable", 10)
            }
            DomainError::UploadConcurrency => unavailable("Too many concurrent uploads", 5),
            DomainError::Contention(cause) => {
                tracing::warn!(cause = %cause, "database contention");
                unavailable("Service temporarily unavailable", 1)
            }
            DomainError::OData(e) => CanonicalError::from(e),
            DomainError::Internal(d) => {
                tracing::error!(diagnostic = %d, "internal error");
                CanonicalError::internal(d).create()
            }
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
