//! Domain error -> canonical error mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::DomainError;

/// `gts.cf.core.mini_chat.chat.v1~`
pub const CHAT_RESOURCE_TYPE: &str = toolkit_gts::gts_id!("cf.core.mini_chat.chat.v1~");
/// `gts.cf.core.mini_chat.message.v1~`
pub const MESSAGE_RESOURCE_TYPE: &str = toolkit_gts::gts_id!("cf.core.mini_chat.message.v1~");
/// `gts.cf.core.mini_chat.turn.v1~`
pub const TURN_RESOURCE_TYPE: &str = toolkit_gts::gts_id!("cf.core.mini_chat.turn.v1~");
/// `gts.cf.core.mini_chat.attachment.v1~`
pub const ATTACHMENT_RESOURCE_TYPE: &str = toolkit_gts::gts_id!("cf.core.mini_chat.attachment.v1~");
/// `gts.cf.core.mini_chat.model.v1~`
pub const MODEL_RESOURCE_TYPE: &str = toolkit_gts::gts_id!("cf.core.mini_chat.model.v1~");

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

#[allow(clippy::too_many_lines, reason = "flat mapping table")]
impl From<DomainError> for CanonicalError {
    #[allow(clippy::cognitive_complexity, reason = "flat mapping table")]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::ChatNotFound(id) => ChatResourceError::not_found("Chat not found")
                .with_resource(id.to_string())
                .create(),
            DomainError::MessageNotFound(id) => MessageResourceError::not_found("Message not found")
                .with_resource(id.to_string())
                .create(),
            DomainError::TurnNotFound(id) => TurnResourceError::not_found("Turn not found")
                .with_resource(id.to_string())
                .create(),
            DomainError::AttachmentNotFound(id) => {
                AttachmentResourceError::not_found("Attachment not found")
                    .with_resource(id.to_string())
                    .create()
            }
            DomainError::ModelNotFound(id) => ModelResourceError::not_found("Model not found")
                .with_resource(id)
                .create(),
            DomainError::InvalidModel(m) => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "model",
                    format!("model `{m}` is not available in the model catalog"),
                    "INVALID_MODEL",
                )
                .create(),
            DomainError::EmptyContent => MessageResourceError::invalid_argument()
                .with_field_violation("content", "content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle(d) => ChatResourceError::invalid_argument()
                .with_field_violation("title", d, "INVALID_TITLE")
                .create(),
            DomainError::InvalidReaction(d) => MessageResourceError::invalid_argument()
                .with_field_violation("reaction", d, "INVALID_REACTION")
                .create(),
            DomainError::InvalidAttachment(d) => AttachmentResourceError::invalid_argument()
                .with_field_violation("attachment", d, "invalid_attachment")
                .create(),
            DomainError::UnsupportedContentType { field, content_type } => {
                AttachmentResourceError::invalid_argument()
                    .with_field_violation(
                        field,
                        format!("unsupported content type `{content_type}`"),
                        "UNSUPPORTED_CONTENT_TYPE",
                    )
                    .create()
            }
            DomainError::CodeInterpreterUnavailable => AttachmentResourceError::invalid_argument()
                .with_field_violation(
                    "file",
                    "code interpreter is not available for this chat",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { field, reason, detail } => {
                AttachmentResourceError::invalid_argument()
                    .with_field_violation(field, detail, reason)
                    .create()
            }
            DomainError::VisionNotSupported(m) => MessageResourceError::invalid_argument()
                .with_field_violation(
                    "content_type",
                    format!("model `{m}` does not support image input"),
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::FileTooLarge { limit_bytes } => {
                let d = format!("file exceeds the upload limit of {limit_bytes} bytes");
                AttachmentResourceError::out_of_range(d.clone())
                    .with_field_violation("content_length", d, "FILE_TOO_LARGE")
                    .create()
            }
            DomainError::TooManyImages { count, max } => {
                let d = format!("{count} images exceed the per-message limit of {max}");
                MessageResourceError::out_of_range(d.clone())
                    .with_field_violation("image_count", d, "TOO_MANY_IMAGES")
                    .create()
            }
            DomainError::InputTooLong { estimated, max } => {
                let d = format!("message is too long ({estimated} estimated tokens > {max})");
                MessageResourceError::out_of_range(d.clone())
                    .with_field_violation("content", d, "INPUT_TOO_LONG")
                    .create()
            }
            DomainError::ContextBudgetExceeded(d) => MessageResourceError::out_of_range(d.clone())
                .with_field_violation("content", d, "CONTEXT_BUDGET_EXCEEDED")
                .create(),
            DomainError::FeatureDisabled(subject) => ChatResourceError::failed_precondition()
                .with_precondition_violation(
                    subject,
                    format!("{subject} is disabled by the operator"),
                    "FEATURE_DISABLED",
                )
                .create(),
            DomainError::TurnNotTerminal => TurnResourceError::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "the turn is still running",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResourceError::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "only assistant messages can receive reactions",
                    "STATE",
                )
                .create(),
            DomainError::ChatCleanupPayloadTooLarge(d) => {
                ChatResourceError::invalid_argument().with_format(d).create()
            }
            DomainError::Forbidden => ChatResourceError::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable(cause) => {
                tracing::error!(cause = %cause, "PDP evaluation failed");
                CanonicalError::service_unavailable().with_retry_after_seconds(5).create()
            }
            DomainError::TurnAlreadyRunning => {
                ChatResourceError::aborted("Another turn is already running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict(internal) => {
                tracing::info!(detail = %internal, "request_id conflict");
                TurnResourceError::aborted("The request_id is already used by another turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => {
                TurnResourceError::aborted("Only the latest turn can be modified")
                    .with_reason("NOT_LATEST_TURN")
                    .create()
            }
            DomainError::GenerationInProgress => {
                TurnResourceError::aborted("A generation is already in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResourceError::aborted("The turn is already completed")
                .with_reason("REPLAY")
                .create(),
            DomainError::AttachmentLocked => {
                AttachmentResourceError::already_exists("Attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentResourceError::already_exists(
                "The chat vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation(internal) => {
                tracing::warn!(detail = %internal, "unhandled unique violation");
                ChatResourceError::already_exists("The resource already exists")
                    .with_resource("unique_violation")
                    .create()
            }
            DomainError::QuotaExceeded(scope) => ChatResourceError::resource_exhausted(format!(
                "Quota exceeded ({scope})"
            ))
            .with_quota_violation(scope, "quota_exceeded")
            .create(),
            DomainError::DocumentLimit => AttachmentResourceError::resource_exhausted(
                "Per-chat document limit reached",
            )
            .with_quota_violation("document_limit", "per-chat document limit reached")
            .create(),
            DomainError::StorageLimit => AttachmentResourceError::resource_exhausted(
                "Per-chat storage limit reached",
            )
            .with_quota_violation("storage_limit", "per-chat storage limit reached")
            .create(),
            DomainError::StorageUnavailable(cause) => {
                tracing::warn!(cause = %cause, "storage backend failure");
                CanonicalError::service_unavailable().with_retry_after_seconds(10).create()
            }
            DomainError::UploadConcurrencyLimit => {
                CanonicalError::service_unavailable().with_retry_after_seconds(5).create()
            }
            DomainError::OData(e) => CanonicalError::from(e),
            DomainError::Internal(diag) => CanonicalError::internal(diag).create(),
            DomainError::Contention(diag) => {
                tracing::warn!(cause = %diag, "database contention persisted after retries");
                CanonicalError::service_unavailable().with_retry_after_seconds(1).create()
            }
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
