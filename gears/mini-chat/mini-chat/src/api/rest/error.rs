//! `DomainError` → canonical error / RFC 9457 `Problem` (ADR-0004).
//!
//! Detail texts are generic: they never carry provider identifiers, internal
//! ids or driver messages (those are logged).

use toolkit_canonical_errors::{CanonicalError, Problem, resource_error};
use tracing::error;

use crate::domain::error::DomainError;

#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatError;

#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageError;

#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnError;

#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentError;

#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelError;

/// Retry hint of PDP / upload-concurrency outages (seconds).
pub const RETRY_AFTER_AUTHZ_SECS: u64 = 5;
/// Retry hint of storage backend outages (seconds).
pub const RETRY_AFTER_STORAGE_SECS: u64 = 10;
/// Retry hint of the upload concurrency limit (seconds).
pub const RETRY_AFTER_UPLOAD_SECS: u64 = 5;

fn unavailable(seconds: u64, detail: &str) -> CanonicalError {
    CanonicalError::service_unavailable()
        .with_retry_after_seconds(seconds)
        .with_detail(detail)
        .create()
}

fn internal(e: &DomainError) -> CanonicalError {
    error!(error = %e, "internal error");
    CanonicalError::internal(e.to_string()).create()
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines)] // one arm per ADR-0004 row
    fn from(e: DomainError) -> Self {
        match e {
            // --- not_found ---
            DomainError::ChatNotFound => ChatError::not_found("Chat not found")
                .with_resource("chat")
                .create(),
            DomainError::MessageNotFound => MessageError::not_found("Message not found")
                .with_resource("message")
                .create(),
            DomainError::TurnNotFound => TurnError::not_found("Turn not found")
                .with_resource("turn")
                .create(),
            DomainError::AttachmentNotFound => AttachmentError::not_found("Attachment not found")
                .with_resource("attachment")
                .create(),
            DomainError::ModelNotFound => ModelError::not_found("Model not found")
                .with_resource("model")
                .create(),

            // --- invalid_argument ---
            DomainError::InvalidModel => ChatError::invalid_argument()
                .with_field_violation("model", "Unknown or unavailable model", "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => MessageError::invalid_argument()
                .with_field_violation("content", "Content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatError::invalid_argument()
                .with_field_violation(
                    "title",
                    "Title must be 1 to 255 characters after trimming",
                    "INVALID_TITLE",
                )
                .create(),
            DomainError::InvalidReaction => MessageError::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "Reaction must be 'like' or 'dislike'",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment => MessageError::invalid_argument()
                .with_field_violation(
                    "attachment",
                    "Invalid, duplicate, foreign or not ready attachment",
                    "invalid_attachment",
                )
                .create(),
            DomainError::UnsupportedContentType => AttachmentError::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "Unsupported file type",
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentError::invalid_argument()
                .with_field_violation(
                    "file",
                    "This file type requires code interpreter, which is unavailable",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { field, reason } => AttachmentError::invalid_argument()
                .with_field_violation(field, "Invalid multipart request", reason)
                .create(),
            DomainError::VisionNotSupported => MessageError::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "The chat's model does not accept images",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::OutboxPayloadTooLarge(msg) => {
                ChatError::invalid_argument().with_format(msg).create()
            }
            // Platform mapping: `gts.cf.core.odata.query.v1~` + field violation.
            DomainError::InvalidQuery(q) => CanonicalError::from(q.0),

            // --- out_of_range ---
            DomainError::FileTooLarge => AttachmentError::out_of_range("File too large")
                .with_field_violation(
                    "content_length",
                    "File exceeds the size limit",
                    "FILE_TOO_LARGE",
                )
                .create(),
            DomainError::TooManyImages => MessageError::out_of_range("Too many images")
                .with_field_violation(
                    "image_count",
                    "Too many images in one message",
                    "TOO_MANY_IMAGES",
                )
                .create(),
            DomainError::InputTooLong => MessageError::out_of_range("Message too long")
                .with_field_violation(
                    "content",
                    "Message exceeds the model input limit",
                    "INPUT_TOO_LONG",
                )
                .create(),
            DomainError::ContextBudgetExceeded => {
                MessageError::out_of_range("Context budget exceeded")
                    .with_field_violation(
                        "content",
                        "Mandatory context does not fit the model budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }

            // --- failed_precondition ---
            DomainError::FeatureDisabled(subject) => MessageError::failed_precondition()
                .with_precondition_violation(
                    subject.as_str(),
                    "Feature is disabled",
                    "FEATURE_DISABLED",
                )
                .create(),
            DomainError::TurnNotTerminal => TurnError::failed_precondition()
                .with_precondition_violation("turn_state", "Turn is still running", "STATE")
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageError::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "Only assistant messages can be reacted to",
                    "STATE",
                )
                .create(),

            // --- permission_denied / service_unavailable ---
            DomainError::AuthzDenied => ChatError::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable => unavailable(
                RETRY_AFTER_AUTHZ_SECS,
                "Authorization service temporarily unavailable",
            ),

            // --- aborted ---
            DomainError::TurnAlreadyRunning => {
                TurnError::aborted("Another turn is running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict => TurnError::aborted("Request id conflict")
                .with_reason("request_id_conflict")
                .create(),
            DomainError::NotLatestTurn => TurnError::aborted("Only the latest turn can be changed")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => TurnError::aborted("A generation is in progress")
                .with_reason("GENERATION_IN_PROGRESS")
                .create(),
            DomainError::Replay => TurnError::aborted("Turn already completed")
                .with_reason("REPLAY")
                .create(),

            // --- already_exists ---
            DomainError::AttachmentLocked => {
                AttachmentError::already_exists("Attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentError::already_exists(
                "The chat's document store belongs to another provider",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation(ref msg) => {
                error!(error = %msg, "unhandled unique violation");
                ChatError::already_exists("Resource already exists")
                    .with_resource("unique_violation")
                    .create()
            }

            // --- resource_exhausted ---
            DomainError::QuotaExceeded(scope) => MessageError::resource_exhausted("Quota exceeded")
                .with_quota_violation(scope.as_str(), "quota_exceeded")
                .create(),
            DomainError::DocumentLimit => {
                AttachmentError::resource_exhausted("Document limit reached")
                    .with_quota_violation("document_limit", "quota_exceeded")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentError::resource_exhausted("Storage limit reached")
                    .with_quota_violation("storage_limit", "quota_exceeded")
                    .create()
            }

            // --- service_unavailable ---
            DomainError::StorageUnavailable => unavailable(
                RETRY_AFTER_STORAGE_SECS,
                "File storage temporarily unavailable",
            ),
            DomainError::UploadConcurrencyLimit => {
                unavailable(RETRY_AFTER_UPLOAD_SECS, "Too many concurrent uploads")
            }

            // --- internal ---
            DomainError::PluginUnavailable(_)
            | DomainError::ProviderResolution(_)
            | DomainError::Internal(_)
            | DomainError::Database(_)
            | DomainError::Db(_) => internal(&e),
        }
    }
}

impl From<DomainError> for Problem {
    fn from(e: DomainError) -> Self {
        Problem::from(CanonicalError::from(e))
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
