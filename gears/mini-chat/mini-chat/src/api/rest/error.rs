//! `DomainError` to canonical error mapping (ADR-0004, DESIGN section 3.3 "Error Codes").
//!
//! The HTTP status follows from the canonical category; the machine-readable reason is in
//! `context.reason`, `context.field_violations[].reason` or `context.violations[]`. Texts are not
//! contract. Operator-facing causes are only logged; clients get generic details.

use toolkit_canonical_errors::{CanonicalError, resource_error};
use tracing::error;

use crate::domain::error::{
    DisabledFeature, DomainError, MultipartIssue, ProtectedResource, QuotaScope,
};

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

#[resource_error(gts_id!("cf.core.mini_chat.user_quota.v1~"))]
struct UserQuotaResource;

/// `Retry-After` of a PDP outage, in seconds.
const AUTHZ_RETRY_AFTER_SECONDS: u64 = 5;
/// `Retry-After` of a storage backend failure on upload, in seconds.
const STORAGE_RETRY_AFTER_SECONDS: u64 = 10;
/// `Retry-After` of the upload concurrency limit, in seconds.
const UPLOAD_RETRY_AFTER_SECONDS: u64 = 5;

/// Detail of every internal failure: the cause is logged, never sent.
const OPAQUE_INTERNAL: &str = "An internal error occurred. Please retry later.";

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::ChatNotFound { chat_id } => ChatResource::not_found("Chat not found")
                .with_resource(chat_id.to_string())
                .create(),
            DomainError::MessageNotFound { message_id } => {
                MessageResource::not_found("Message not found")
                    .with_resource(message_id.to_string())
                    .create()
            }
            DomainError::TurnNotFound { request_id } => TurnResource::not_found("Turn not found")
                .with_resource(request_id.to_string())
                .create(),
            DomainError::AttachmentNotFound { attachment_id } => {
                AttachmentResource::not_found("Attachment not found")
                    .with_resource(attachment_id.to_string())
                    .create()
            }
            DomainError::ModelNotFound { model_id } => ModelResource::not_found("Model not found")
                .with_resource(model_id)
                .create(),
            DomainError::AuthzDenied { resource } => permission_denied(resource),
            DomainError::AuthzUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(AUTHZ_RETRY_AFTER_SECONDS)
                .with_detail("Authorization is temporarily unavailable")
                .create(),
            DomainError::Odata(err) => CanonicalError::from(err),
            DomainError::PolicyUnavailable { diagnostic }
            | DomainError::Internal { diagnostic } => {
                error!(diagnostic = %diagnostic, "internal error");
                CanonicalError::internal(OPAQUE_INTERNAL).create()
            }
            other => map_request_error(other),
        }
    }
}

fn permission_denied(resource: ProtectedResource) -> CanonicalError {
    match resource {
        ProtectedResource::Chat => ChatResource::permission_denied()
            .with_reason("AUTHZ_DENIED")
            .create(),
        ProtectedResource::Model => ModelResource::permission_denied()
            .with_reason("AUTHZ_DENIED")
            .create(),
        ProtectedResource::UserQuota => UserQuotaResource::permission_denied()
            .with_reason("AUTHZ_DENIED")
            .create(),
    }
}

/// Request-validation, conflict and limit errors.
fn map_request_error(err: DomainError) -> CanonicalError {
    match err {
        DomainError::InvalidModel { detail } => ChatResource::invalid_argument()
            .with_field_violation("model", detail, "INVALID_MODEL")
            .create(),
        DomainError::InvalidTitle { detail } => ChatResource::invalid_argument()
            .with_field_violation("title", detail, "INVALID_TITLE")
            .create(),
        DomainError::EmptyContent => ChatResource::invalid_argument()
            .with_field_violation("content", "content must not be empty", "EMPTY_CONTENT")
            .create(),
        DomainError::InvalidReaction => MessageResource::invalid_argument()
            .with_field_violation(
                "reaction",
                "reaction must be like or dislike",
                "INVALID_REACTION",
            )
            .create(),
        DomainError::UnsupportedContentType => AttachmentResource::invalid_argument()
            .with_field_violation(
                "content_type",
                "the content type is not supported",
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
        DomainError::InvalidMultipart { issue } => multipart_error(issue),
        DomainError::VisionNotSupported => ChatResource::invalid_argument()
            .with_field_violation(
                "content_type",
                "the effective model does not support image input",
                "VISION_NOT_SUPPORTED",
            )
            .create(),
        DomainError::InvalidAttachment => ChatResource::invalid_argument()
            .with_field_violation(
                "attachment",
                "an attachment id is invalid, duplicated, foreign or not ready",
                "invalid_attachment",
            )
            .create(),
        DomainError::OutboxPayloadTooLarge { detail } => ChatResource::invalid_argument()
            .with_format(detail)
            .create(),
        DomainError::FileTooLarge => AttachmentResource::out_of_range("File too large")
            .with_field_violation(
                "content_length",
                "the file exceeds the size limit",
                "FILE_TOO_LARGE",
            )
            .create(),
        DomainError::TooManyImages => ChatResource::out_of_range("Too many images")
            .with_field_violation(
                "image_count",
                "too many images in one message",
                "TOO_MANY_IMAGES",
            )
            .create(),
        DomainError::InputTooLong => ChatResource::out_of_range("Input too long")
            .with_field_violation(
                "content",
                "the message exceeds the input token limit",
                "INPUT_TOO_LONG",
            )
            .create(),
        DomainError::ContextBudgetExceeded => ChatResource::out_of_range("Context budget exceeded")
            .with_field_violation(
                "context",
                "the mandatory context does not fit the budget",
                "CONTEXT_BUDGET_EXCEEDED",
            )
            .create(),
        other => map_state_error(other),
    }
}

fn multipart_error(issue: MultipartIssue) -> CanonicalError {
    let (field, description, reason) = match issue {
        MultipartIssue::BoundaryRequired => (
            "content_type",
            "the multipart boundary is missing",
            "BOUNDARY_REQUIRED",
        ),
        MultipartIssue::Malformed => (
            "multipart",
            "the multipart body is unreadable",
            "MULTIPART_ERROR",
        ),
        MultipartIssue::MissingFile => ("file", "the request has no file field", "MISSING_FILE"),
        MultipartIssue::MissingContentType => (
            "content_type",
            "the file part has no content type",
            "MISSING_CONTENT_TYPE",
        ),
    };
    AttachmentResource::invalid_argument()
        .with_field_violation(field, description, reason)
        .create()
}

/// Precondition, conflict and quota errors.
fn map_state_error(err: DomainError) -> CanonicalError {
    match err {
        DomainError::FeatureDisabled { feature } => {
            let subject = match feature {
                DisabledFeature::WebSearch => "web_search",
                DisabledFeature::Images => "images",
            };
            ChatResource::failed_precondition()
                .with_precondition_violation(subject, "the feature is disabled", "FEATURE_DISABLED")
                .create()
        }
        DomainError::InvalidTurnState => TurnResource::failed_precondition()
            .with_precondition_violation(
                "turn_state",
                "the turn is not in a terminal state",
                "STATE",
            )
            .create(),
        DomainError::InvalidReactionTarget => MessageResource::failed_precondition()
            .with_precondition_violation(
                "reaction_target",
                "reactions apply to assistant messages only",
                "STATE",
            )
            .create(),
        DomainError::TurnAlreadyRunning => {
            aborted("Another turn is running", "turn_already_running")
        }
        DomainError::RequestIdConflict => {
            aborted("The request id is already in use", "request_id_conflict")
        }
        DomainError::NotLatestTurn => {
            aborted("Only the latest turn can be mutated", "NOT_LATEST_TURN")
        }
        DomainError::GenerationInProgress => {
            aborted("A generation is in progress", "GENERATION_IN_PROGRESS")
        }
        DomainError::Replay => aborted("The turn is already completed", "REPLAY"),
        DomainError::AttachmentLocked => AttachmentResource::already_exists("Attachment is in use")
            .with_resource("attachment_locked")
            .create(),
        DomainError::ProviderMismatch => {
            AttachmentResource::already_exists("Storage provider mismatch")
                .with_resource("provider_mismatch")
                .create()
        }
        DomainError::UniqueViolation => {
            ChatResource::already_exists("A conflicting resource exists")
                .with_resource("unique_violation")
                .create()
        }
        other => map_limit_error(other),
    }
}

fn aborted(detail: &str, reason: &str) -> CanonicalError {
    ChatResource::aborted(detail).with_reason(reason).create()
}

/// Quota, limit and availability errors.
fn map_limit_error(err: DomainError) -> CanonicalError {
    match err {
        DomainError::QuotaExceeded { scope } => {
            let subject = match scope {
                QuotaScope::Tokens => "tokens",
                QuotaScope::WebSearch => "web_search",
                QuotaScope::CodeInterpreter => "code_interpreter",
            };
            ChatResource::resource_exhausted("Quota exceeded")
                .with_quota_violation(subject, "quota_exceeded")
                .create()
        }
        DomainError::DocumentLimit => {
            AttachmentResource::resource_exhausted("Document limit reached")
                .with_quota_violation("document_limit", "document_limit")
                .create()
        }
        DomainError::StorageLimit => {
            AttachmentResource::resource_exhausted("Storage limit reached")
                .with_quota_violation("storage_limit", "storage_limit")
                .create()
        }
        DomainError::StorageUnavailable => CanonicalError::service_unavailable()
            .with_retry_after_seconds(STORAGE_RETRY_AFTER_SECONDS)
            .with_detail("The storage backend is temporarily unavailable")
            .create(),
        DomainError::UploadConcurrencyLimit => CanonicalError::service_unavailable()
            .with_retry_after_seconds(UPLOAD_RETRY_AFTER_SECONDS)
            .with_detail("Too many concurrent uploads")
            .create(),
        other => {
            error!(error = %other, "unmapped domain error");
            CanonicalError::internal(OPAQUE_INTERNAL).create()
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
