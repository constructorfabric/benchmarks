//! `DomainError` → canonical `Problem` mapping (ADR-0004).
//!
//! The category fixes the HTTP status; the machine-readable reason is placed
//! in `context.reason`, `context.field_violations[].reason` or
//! `context.violations[]`. `detail` texts are generic and never carry
//! provider identifiers or driver messages.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, MultipartFailure};

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResourceError;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResourceError;

/// Retry-After of a PDP evaluation failure.
pub const AUTHZ_RETRY_AFTER_SECS: u64 = 5;
/// Retry-After of a storage backend failure on upload.
pub const STORAGE_RETRY_AFTER_SECS: u64 = 10;
/// Retry-After of the upload concurrency limit.
pub const UPLOAD_CONCURRENCY_RETRY_AFTER_SECS: u64 = 5;

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::ChatNotFound { id } => ChatResourceError::not_found("Chat not found")
                .with_resource(id.to_string())
                .create(),
            DomainError::MessageNotFound { id } => {
                MessageResourceError::not_found("Message not found")
                    .with_resource(id.to_string())
                    .create()
            }
            DomainError::TurnNotFound { request_id } => {
                TurnResourceError::not_found("Turn not found")
                    .with_resource(request_id.to_string())
                    .create()
            }
            DomainError::AttachmentNotFound { id } => {
                AttachmentResourceError::not_found("Attachment not found")
                    .with_resource(id.to_string())
                    .create()
            }
            DomainError::ModelNotFound { id } => ModelResourceError::not_found("Model not found")
                .with_resource(id)
                .create(),

            DomainError::InvalidModel { .. } => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "model",
                    "The model is unknown, disabled or no longer in the catalog",
                    "INVALID_MODEL",
                )
                .create(),
            DomainError::InvalidTitle => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "title",
                    "Title must be 1-255 characters after trimming",
                    "INVALID_TITLE",
                )
                .create(),
            DomainError::EmptyContent => MessageResourceError::invalid_argument()
                .with_field_violation("content", "Message content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidReaction => MessageResourceError::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "Reaction must be 'like' or 'dislike'",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment { detail } => {
                tracing::debug!(%detail, "mini-chat: invalid attachment_ids");
                AttachmentResourceError::invalid_argument()
                    .with_field_violation(
                        "attachment",
                        "Attachment ids must be unique, ready attachments of this chat",
                        "invalid_attachment",
                    )
                    .create()
            }
            DomainError::UnsupportedContentType { content_type } => {
                AttachmentResourceError::invalid_argument()
                    .with_field_violation(
                        "content_type",
                        format!("Unsupported file type: {content_type}"),
                        "UNSUPPORTED_CONTENT_TYPE",
                    )
                    .create()
            }
            DomainError::CodeInterpreterUnavailable => AttachmentResourceError::invalid_argument()
                .with_field_violation(
                    "file",
                    "This file type requires the code interpreter, which is not available for this chat",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { failure, detail } => {
                let (field, reason) = match failure {
                    MultipartFailure::BoundaryRequired => ("content_type", "BOUNDARY_REQUIRED"),
                    MultipartFailure::Unreadable => ("multipart", "MULTIPART_ERROR"),
                    MultipartFailure::MissingFile => ("file", "MISSING_FILE"),
                    MultipartFailure::MissingContentType => ("content_type", "MISSING_CONTENT_TYPE"),
                };
                AttachmentResourceError::invalid_argument()
                    .with_field_violation(field, detail, reason)
                    .create()
            }
            DomainError::OutboxPayloadTooLarge { detail } => ChatResourceError::invalid_argument()
                .with_format(detail)
                .create(),
            DomainError::VisionNotSupported => MessageResourceError::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "The model used for this turn does not support image input",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),

            DomainError::FileTooLarge { limit_bytes } => {
                AttachmentResourceError::out_of_range("File too large")
                    .with_field_violation(
                        "content_length",
                        format!("File exceeds the maximum size of {limit_bytes} bytes"),
                        "FILE_TOO_LARGE",
                    )
                    .create()
            }
            DomainError::TooManyImages { max } => {
                MessageResourceError::out_of_range("Too many images")
                    .with_field_violation(
                        "image_count",
                        format!("At most {max} images are allowed per message"),
                        "TOO_MANY_IMAGES",
                    )
                    .create()
            }
            DomainError::InputTooLong { limit } => {
                MessageResourceError::out_of_range("Message too long")
                    .with_field_violation(
                        "content",
                        format!("The message exceeds the model input limit of {limit} tokens"),
                        "INPUT_TOO_LONG",
                    )
                    .create()
            }
            DomainError::ContextBudgetExceeded => {
                MessageResourceError::out_of_range("Context budget exceeded")
                    .with_field_violation(
                        "content",
                        "The mandatory context does not fit the model token budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }

            DomainError::FeatureDisabled { feature } => ChatResourceError::failed_precondition()
                .with_precondition_violation(
                    feature.as_str(),
                    format!("The {} feature is disabled", feature.as_str()),
                    "FEATURE_DISABLED",
                )
                .create(),
            DomainError::TurnNotTerminal => TurnResourceError::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "The turn is still running",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResourceError::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "Reactions are allowed on assistant messages only",
                    "STATE",
                )
                .create(),

            DomainError::AccessDenied => ChatResourceError::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable { .. } => CanonicalError::service_unavailable()
                .with_retry_after_seconds(AUTHZ_RETRY_AFTER_SECS)
                .create(),

            DomainError::TurnAlreadyRunning => {
                ChatResourceError::aborted("A response is already being generated in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict { detail } => {
                tracing::info!(%detail, "mini-chat: request_id conflict");
                ChatResourceError::aborted("The request_id is already used by another turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => {
                TurnResourceError::aborted("Only the latest turn can be changed")
                    .with_reason("NOT_LATEST_TURN")
                    .create()
            }
            DomainError::GenerationInProgress => {
                TurnResourceError::aborted("A concurrent generation is in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResourceError::aborted("The turn is already completed")
                .with_reason("REPLAY")
                .create(),

            DomainError::AttachmentLocked => {
                AttachmentResourceError::already_exists("The attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentResourceError::already_exists(
                "The chat documents are stored with another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation { detail } => {
                tracing::warn!(%detail, "mini-chat: unhandled unique violation");
                ChatResourceError::already_exists("The resource already exists")
                    .with_resource("unique_violation")
                    .create()
            }

            DomainError::QuotaExceeded { scope } => {
                ChatResourceError::resource_exhausted("Quota exceeded")
                    .with_quota_violation(scope.as_str(), "quota_exceeded")
                    .create()
            }
            DomainError::DocumentLimit => {
                AttachmentResourceError::resource_exhausted("Per-chat document limit reached")
                    .with_quota_violation("document_limit", "Too many documents in this chat")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentResourceError::resource_exhausted("Per-chat storage limit reached")
                    .with_quota_violation(
                        "storage_limit",
                        "The total size of the chat attachments exceeds the limit",
                    )
                    .create()
            }

            DomainError::StorageUnavailable { detail } => {
                tracing::warn!(%detail, "mini-chat: storage backend unavailable");
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(STORAGE_RETRY_AFTER_SECS)
                    .create()
            }
            DomainError::UploadConcurrencyLimit => CanonicalError::service_unavailable()
                .with_retry_after_seconds(UPLOAD_CONCURRENCY_RETRY_AFTER_SECS)
                .create(),

            DomainError::ProviderResolution { detail }
            | DomainError::PolicyResolution { detail }
            | DomainError::MessagePersistence { detail }
            | DomainError::Internal { detail } => CanonicalError::internal(detail).create(),
            DomainError::Database(e) => CanonicalError::internal(format!("database error: {e}")).create(),
            DomainError::OData(e) => CanonicalError::from(e),
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
