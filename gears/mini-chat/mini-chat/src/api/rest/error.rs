//! Domain error → canonical `Problem` mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, Resource};

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResource;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResource;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResource;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResource;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResource;

fn not_found(resource: Resource) -> CanonicalError {
    match resource {
        Resource::Chat => ChatResource::not_found("Chat not found")
            .with_resource("chat")
            .create(),
        Resource::Message => MessageResource::not_found("Message not found")
            .with_resource("message")
            .create(),
        Resource::Turn => TurnResource::not_found("Turn not found")
            .with_resource("turn")
            .create(),
        Resource::Attachment => AttachmentResource::not_found("Attachment not found")
            .with_resource("attachment")
            .create(),
        Resource::Model => ModelResource::not_found("Model not found")
            .with_resource("model")
            .create(),
    }
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines)]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound(r) => not_found(r),
            DomainError::InvalidModel(detail) => ChatResource::invalid_argument()
                .with_field_violation("model", detail, "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => MessageResource::invalid_argument()
                .with_field_violation("content", "content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatResource::invalid_argument()
                .with_field_violation(
                    "title",
                    "title must be 1-255 characters after trimming",
                    "INVALID_TITLE",
                )
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "reaction must be 'like' or 'dislike'",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment(detail) => MessageResource::invalid_argument()
                .with_field_violation("attachment", detail, "invalid_attachment")
                .create(),
            DomainError::UnsupportedContentType(ct) => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    format!("unsupported content type '{ct}'"),
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "code interpreter is unavailable for this chat",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart {
                field,
                reason,
                detail,
            } => AttachmentResource::invalid_argument()
                .with_field_violation(field, detail, reason)
                .create(),
            DomainError::VisionNotSupported => MessageResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "the model does not support image input",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::FileTooLarge { limit_bytes } => {
                AttachmentResource::out_of_range("File too large")
                    .with_field_violation(
                        "content_length",
                        format!("file exceeds the limit of {limit_bytes} bytes"),
                        "FILE_TOO_LARGE",
                    )
                    .create()
            }
            DomainError::TooManyImages { max } => MessageResource::out_of_range("Too many images")
                .with_field_violation(
                    "image_count",
                    format!("at most {max} images per message"),
                    "TOO_MANY_IMAGES",
                )
                .create(),
            DomainError::InputTooLong => MessageResource::out_of_range("Message too long")
                .with_field_violation(
                    "content",
                    "message exceeds the model input limit",
                    "INPUT_TOO_LONG",
                )
                .create(),
            DomainError::ContextBudgetExceeded => {
                MessageResource::out_of_range("Context budget exceeded")
                    .with_field_violation(
                        "content",
                        "mandatory context does not fit the model context budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }
            DomainError::FeatureDisabled(f) => ChatResource::failed_precondition()
                .with_precondition_violation(f.subject(), "feature is disabled", "FEATURE_DISABLED")
                .create(),
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "the turn is not in a terminal state",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTarget => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "reactions are allowed on assistant messages only",
                    "STATE",
                )
                .create(),
            DomainError::AuthzDenied => ChatResource::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("Authorization service temporarily unavailable")
                .create(),
            DomainError::TurnAlreadyRunning => {
                ChatResource::aborted("Another turn is running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict(internal) => {
                tracing::info!(detail = %internal, "request_id conflict");
                TurnResource::aborted("The request_id is already used by another turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => {
                TurnResource::aborted("Only the latest turn can be modified")
                    .with_reason("NOT_LATEST_TURN")
                    .create()
            }
            DomainError::GenerationInProgress => {
                TurnResource::aborted("A generation is already in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResource::aborted("Completed turn replay")
                .with_reason("REPLAY")
                .create(),
            DomainError::AttachmentLocked => AttachmentResource::already_exists(
                "Attachment is referenced by a submitted message",
            )
            .with_resource("attachment_locked")
            .create(),
            DomainError::ProviderMismatch => AttachmentResource::already_exists(
                "The chat vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation => {
                ChatResource::already_exists("The resource already exists")
                    .with_resource("unique_violation")
                    .create()
            }
            DomainError::QuotaExceeded(scope) => ChatResource::resource_exhausted("Quota exceeded")
                .with_quota_violation(scope.as_str(), "quota_exceeded")
                .create(),
            DomainError::DocumentLimit => {
                AttachmentResource::resource_exhausted("Per-chat document limit reached")
                    .with_quota_violation("document_limit", "per-chat document count limit reached")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentResource::resource_exhausted("Per-chat storage limit reached")
                    .with_quota_violation(
                        "storage_limit",
                        "per-chat total upload size limit reached",
                    )
                    .create()
            }
            DomainError::StorageUnavailable(internal) => {
                tracing::warn!(detail = %internal, "storage backend failure");
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(10)
                    .with_detail("Service temporarily unavailable")
                    .create()
            }
            DomainError::UploadConcurrency => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("Too many concurrent uploads")
                .create(),
            DomainError::PayloadTooLarge(msg) => {
                ChatResource::invalid_argument().with_format(msg).create()
            }
            DomainError::Internal(msg) | DomainError::Contention(msg) => {
                CanonicalError::internal(msg).create()
            }
        }
    }
}
