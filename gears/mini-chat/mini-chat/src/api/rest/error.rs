//! `DomainError` -> `CanonicalError` mapping (ADR-0004, DESIGN §3.3 "Error Codes").
//!
//! This is the single place every REST error of the gear is produced. The wire
//! `detail` of an error is composed here from fixed text, never from the
//! variant's `Display` or its inner strings, which may carry driver messages
//! or provider details. Causes are logged and not returned.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, UNIQUE_VIOLATION};
use crate::domain::services::ListError;

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

/// `Retry-After` for the PDP being unreachable and for the upload concurrency limit.
const RETRY_AFTER_SHORT_SECS: u64 = 5;
/// `Retry-After` for a storage backend failure.
const RETRY_AFTER_STORAGE_SECS: u64 = 10;

/// Generic `detail` of every 503 (DESIGN §3.6 "File Upload"); causes are only logged.
const GENERIC_UNAVAILABLE: &str = "Service temporarily unavailable";

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)] // one arm per variant, by design
    fn from(err: DomainError) -> Self {
        match err {
            // --- 404 not_found ---
            DomainError::ChatNotFound => ChatResource::not_found("Chat not found")
                .with_resource("chat")
                .create(),
            DomainError::MessageNotFound => MessageResource::not_found("Message not found")
                .with_resource("message")
                .create(),
            DomainError::TurnNotFound => TurnResource::not_found("Turn not found")
                .with_resource("turn")
                .create(),
            DomainError::AttachmentNotFound => {
                AttachmentResource::not_found("Attachment not found")
                    .with_resource("attachment")
                    .create()
            }
            DomainError::ModelNotFound => ModelResource::not_found("Model not found")
                .with_resource("model")
                .create(),

            // --- 400 invalid_argument ---
            DomainError::InvalidModel => ModelResource::invalid_argument()
                .with_field_violation(
                    "model",
                    "Model is unknown, disabled or no longer available",
                    "INVALID_MODEL",
                )
                .create(),
            DomainError::EmptyContent => MessageResource::invalid_argument()
                .with_field_violation("content", "Content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatResource::invalid_argument()
                .with_field_violation(
                    "title",
                    "Title must be 1 to 255 characters after trimming",
                    "INVALID_TITLE",
                )
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "Reaction must be 'like' or 'dislike'",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment(why) => AttachmentResource::invalid_argument()
                .with_field_violation("attachment", why, "invalid_attachment")
                .create(),
            DomainError::VisionNotSupported => MessageResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "The chat model does not support images",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::UnsupportedContentType(ct) => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    format!("Content type '{ct}' is not supported"),
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "Code interpreter is unavailable for this chat",
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
            DomainError::OutboxPayloadTooLarge(msg) => {
                ChatResource::invalid_argument().with_format(msg).create()
            }

            // --- 400 out_of_range ---
            DomainError::TooManyImages { max } => {
                MessageResource::out_of_range("Too many images in one message")
                    .with_field_violation(
                        "image_count",
                        format!("At most {max} images are allowed per message"),
                        "TOO_MANY_IMAGES",
                    )
                    .create()
            }
            DomainError::InputTooLong => MessageResource::out_of_range("Message is too long")
                .with_field_violation(
                    "content",
                    "Message exceeds the maximum input size",
                    "INPUT_TOO_LONG",
                )
                .create(),
            DomainError::ContextBudgetExceeded => {
                MessageResource::out_of_range("Context does not fit the token budget")
                    .with_field_violation(
                        "content",
                        "Mandatory context does not fit the token budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }
            DomainError::FileTooLarge { limit_bytes } => {
                AttachmentResource::out_of_range("File is too large")
                    .with_field_violation(
                        "content_length",
                        format!("File exceeds the limit of {limit_bytes} bytes"),
                        "FILE_TOO_LARGE",
                    )
                    .create()
            }

            // --- 400 failed_precondition ---
            DomainError::FeatureDisabled { subject } => ChatResource::failed_precondition()
                .with_precondition_violation(
                    subject,
                    format!("The '{subject}' feature is disabled"),
                    "FEATURE_DISABLED",
                )
                .create(),
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "The turn is not in a terminal state",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "Reactions are only allowed on assistant messages",
                    "STATE",
                )
                .create(),

            // --- 403 permission_denied ---
            DomainError::PermissionDenied => ChatResource::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),

            // --- 409 aborted ---
            DomainError::TurnAlreadyRunning => {
                TurnResource::aborted("Another turn is already running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict => {
                TurnResource::aborted("The request_id conflicts with an existing turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => TurnResource::aborted("The turn is not the latest turn")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => {
                TurnResource::aborted("A generation is in progress for this chat")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResource::aborted("The turn has already completed")
                .with_reason("REPLAY")
                .create(),

            // --- 409 already_exists ---
            DomainError::AttachmentLocked => {
                AttachmentResource::already_exists("The attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => ChatResource::already_exists(
                "The chat's vector store was created for another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::Conflict { code } => {
                let detail = if code == UNIQUE_VIOLATION {
                    "The resource already exists"
                } else {
                    "The request conflicts with the current state"
                };
                ChatResource::already_exists(detail)
                    .with_resource(code)
                    .create()
            }

            // --- 429 resource_exhausted ---
            DomainError::QuotaExceeded { scope } => {
                ChatResource::resource_exhausted("Quota exceeded")
                    .with_quota_violation(scope.as_str(), "quota_exceeded")
                    .create()
            }
            DomainError::DocumentLimit => {
                ChatResource::resource_exhausted("Document limit reached")
                    .with_quota_violation(
                        "document_limit",
                        "The chat has reached its maximum number of documents",
                    )
                    .create()
            }
            DomainError::StorageLimit => ChatResource::resource_exhausted("Storage limit reached")
                .with_quota_violation(
                    "storage_limit",
                    "The chat has reached its maximum document storage size",
                )
                .create(),

            // --- 503 service_unavailable ---
            DomainError::AuthzUnavailable => {
                tracing::warn!("authorization could not be evaluated; refusing (fail-closed)");
                unavailable(RETRY_AFTER_SHORT_SECS)
            }
            DomainError::StorageUnavailable(cause) => {
                tracing::warn!(%cause, "storage backend failure");
                unavailable(RETRY_AFTER_STORAGE_SECS)
            }
            DomainError::UploadConcurrencyLimit => unavailable(RETRY_AFTER_SHORT_SECS),

            // --- 500 internal ---
            DomainError::Internal(cause) | DomainError::DbContention(cause) => {
                tracing::error!(%cause, "internal error");
                CanonicalError::internal(cause).create()
            }
        }
    }
}

impl From<ListError> for CanonicalError {
    fn from(err: ListError) -> Self {
        match err {
            // Keeps the OData resource type (`gts.cf.core.odata.query.v1~`).
            ListError::OData(e) => Self::from(e),
            ListError::Domain(e) => Self::from(e),
        }
    }
}

fn unavailable(retry_after_seconds: u64) -> CanonicalError {
    CanonicalError::service_unavailable()
        .with_detail(GENERIC_UNAVAILABLE)
        .with_retry_after_seconds(retry_after_seconds)
        .create()
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
