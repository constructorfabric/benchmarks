//! `DomainError` to canonical `Problem` mapping (ADR-0004).
//!
//! The machine-readable reason is in `context.reason` (`aborted`,
//! `permission_denied`), `context.field_violations[].reason` (`invalid_argument`,
//! `out_of_range`) or `context.violations[]` (`failed_precondition`,
//! `resource_exhausted`). `detail` texts are not contract and never carry
//! driver, provider or id text.

use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::{DomainError, ResourceKind};

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

/// `Retry-After` for a PDP that could not evaluate and for the upload
/// concurrency limit.
const RETRY_AFTER_SHORT_SECS: u64 = 5;
/// `Retry-After` for a failing storage backend.
const RETRY_AFTER_STORAGE_SECS: u64 = 10;

const OUTBOX_TOO_LARGE_MSG: &str = "The chat cleanup payload exceeds the size limit";

fn not_found(kind: ResourceKind) -> CanonicalError {
    let name = kind.name();
    match kind {
        ResourceKind::Chat => ChatResourceError::not_found("Chat not found"),
        ResourceKind::Message => MessageResourceError::not_found("Message not found"),
        ResourceKind::Turn => TurnResourceError::not_found("Turn not found"),
        ResourceKind::Attachment => AttachmentResourceError::not_found("Attachment not found"),
        ResourceKind::Model => ModelResourceError::not_found("Model not found"),
    }
    .with_resource(name)
    .create()
}

impl From<DomainError> for CanonicalError {
    // A flat one-arm-per-variant table; splitting it would only hide the mapping.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::NotFound { resource } => not_found(resource),

            // invalid_argument (400)
            DomainError::InvalidModel => ChatResourceError::invalid_argument()
                .with_field_violation("model", "unknown or disabled model", "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "content",
                    "content must not be empty or whitespace-only",
                    "EMPTY_CONTENT",
                )
                .create(),
            DomainError::InvalidTitle => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "title",
                    "title must be 1..=255 characters after trim",
                    "INVALID_TITLE",
                )
                .create(),
            DomainError::InvalidReaction => MessageResourceError::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "reaction must be `like` or `dislike`",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "attachment",
                    "attachment ids are invalid, duplicated, foreign or not ready",
                    "invalid_attachment",
                )
                .create(),
            DomainError::UnsupportedContentType => AttachmentResourceError::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "unsupported content type",
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResourceError::invalid_argument()
                .with_field_violation(
                    "file",
                    "code interpreter is unavailable for this file",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { reason, field } => AttachmentResourceError::invalid_argument()
                .with_field_violation(field, "invalid multipart upload", reason)
                .create(),
            DomainError::VisionNotSupported => ChatResourceError::invalid_argument()
                .with_field_violation(
                    "model",
                    "the effective model does not support images",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::OutboxPayloadTooLarge {
                during_chat_delete: true,
            } => ChatResourceError::invalid_argument()
                .with_format(OUTBOX_TOO_LARGE_MSG)
                .create(),

            // out_of_range (400)
            DomainError::FileTooLarge => AttachmentResourceError::out_of_range("File too large")
                .with_field_violation(
                    "content_length",
                    "file exceeds the upload size limit",
                    "FILE_TOO_LARGE",
                )
                .create(),
            DomainError::TooManyImages => {
                ChatResourceError::out_of_range("Too many images in one message")
                    .with_field_violation(
                        "image_count",
                        "too many images in one message",
                        "TOO_MANY_IMAGES",
                    )
                    .create()
            }
            DomainError::InputTooLong => ChatResourceError::out_of_range("Message too long")
                .with_field_violation(
                    "content",
                    "message exceeds the input token limit",
                    "INPUT_TOO_LONG",
                )
                .create(),
            DomainError::ContextBudgetExceeded => {
                ChatResourceError::out_of_range("Context does not fit the budget")
                    .with_field_violation(
                        "content",
                        "mandatory context does not fit the budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }

            // failed_precondition (400)
            DomainError::FeatureDisabled { subject } => {
                let description = format!("{subject} is disabled");
                if subject == "images" {
                    AttachmentResourceError::failed_precondition()
                        .with_precondition_violation(subject, description, "FEATURE_DISABLED")
                        .create()
                } else {
                    ChatResourceError::failed_precondition()
                        .with_precondition_violation(subject, description, "FEATURE_DISABLED")
                        .create()
                }
            }
            DomainError::TurnNotTerminal => TurnResourceError::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "the turn is not in a terminal state",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResourceError::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "reactions are only allowed on assistant messages",
                    "STATE",
                )
                .create(),

            // permission_denied (403) / service_unavailable (503)
            DomainError::AuthzDenied => ChatResourceError::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::NotRequester => TurnResourceError::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable | DomainError::UploadConcurrency => {
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(RETRY_AFTER_SHORT_SECS)
                    .create()
            }
            DomainError::StorageUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(RETRY_AFTER_STORAGE_SECS)
                .create(),

            // aborted (409)
            DomainError::TurnAlreadyRunning => {
                TurnResourceError::aborted("Another turn is already running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict => {
                TurnResourceError::aborted("The request id conflicts with an existing turn")
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
            DomainError::Replay => TurnResourceError::aborted("The turn was already completed")
                .with_reason("REPLAY")
                .create(),

            // already_exists (409)
            DomainError::AttachmentLocked => {
                AttachmentResourceError::already_exists("Attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentResourceError::already_exists(
                "The chat's vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation => ChatResourceError::already_exists("Conflict")
                .with_resource("unique_violation")
                .create(),

            // resource_exhausted (429)
            DomainError::QuotaExceeded { scope } => {
                ChatResourceError::resource_exhausted("Quota exceeded")
                    .with_quota_violation(scope.as_str(), "quota_exceeded")
                    .create()
            }
            DomainError::DocumentLimit => {
                AttachmentResourceError::resource_exhausted("Per-chat document limit reached")
                    .with_quota_violation("document_limit", "per-chat document limit reached")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentResourceError::resource_exhausted("Per-chat storage limit reached")
                    .with_quota_violation("storage_limit", "per-chat storage limit reached")
                    .create()
            }

            // invalid_argument (400) on `gts.cf.core.odata.query.v1~`, or 500
            // for a pagination database failure (mapped by toolkit-odata)
            DomainError::Query(q) => CanonicalError::from(q.0),

            // internal (500): the cause is logged and never sent
            DomainError::OutboxPayloadTooLarge {
                during_chat_delete: false,
            } => {
                tracing::error!("outbox payload too large");
                CanonicalError::internal("outbox payload too large").create()
            }
            DomainError::ProviderResolution(msg) => {
                tracing::error!(error = %msg, "provider resolution failed");
                CanonicalError::internal(format!("provider resolution failed: {msg}")).create()
            }
            DomainError::PolicySnapshotGone(msg) => {
                tracing::error!(error = %msg, "policy snapshot not found");
                CanonicalError::internal(format!("policy snapshot not found: {msg}")).create()
            }
            DomainError::Internal(msg) => {
                tracing::error!(error = %msg, "internal error");
                CanonicalError::internal(msg).create()
            }
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
