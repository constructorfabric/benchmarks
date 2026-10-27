//! Domain error type and the canonical error contract mapping (ADR-0004).
//!
//! Every REST error is an RFC 9457 `Problem` whose category determines the
//! HTTP status; machine-readable reasons live in `context.reason`,
//! `context.field_violations[].reason` or `context.violations[]`.

use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

/// Chat resource (`gts.cf.core.mini_chat.chat.v1~`).
#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResource;

/// Message resource (`gts.cf.core.mini_chat.message.v1~`).
#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResource;

/// Turn resource (`gts.cf.core.mini_chat.turn.v1~`).
#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResource;

/// Attachment resource (`gts.cf.core.mini_chat.attachment.v1~`).
#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResource;

/// Model resource (`gts.cf.core.mini_chat.model.v1~`).
#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResource;

/// The missing-resource kind for 404 mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    /// Chat.
    Chat,
    /// Message.
    Message,
    /// Turn.
    Turn,
    /// Attachment.
    Attachment,
    /// Model.
    Model,
}

/// The mini-chat domain error.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// Resource missing, soft-deleted or foreign (404).
    #[error("not found")]
    NotFound(ResourceKind),
    /// Unknown or disabled model (400 `field_violations[model] = INVALID_MODEL`).
    #[error("invalid model")]
    InvalidModel,
    /// Empty or whitespace-only content (400 `EMPTY_CONTENT`).
    #[error("empty content")]
    EmptyContent,
    /// Invalid chat title (400 `INVALID_TITLE`).
    #[error("invalid title")]
    InvalidTitle,
    /// Invalid reaction value (400 `INVALID_REACTION`).
    #[error("invalid reaction")]
    InvalidReaction,
    /// Unsupported upload MIME type (400 `UNSUPPORTED_CONTENT_TYPE`).
    #[error("unsupported content type")]
    UnsupportedContentType,
    /// Code-interpreter-only upload while unavailable (400
    /// `CODE_INTERPRETER_UNAVAILABLE`).
    #[error("code interpreter unavailable")]
    CodeInterpreterUnavailable,
    /// Invalid multipart request (400; field + reason).
    #[error("invalid multipart: {reason}")]
    Multipart {
        /// Violated field (`content_type`, `multipart`, `file`).
        field: &'static str,
        /// Reason code (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`,
        /// `MISSING_CONTENT_TYPE`).
        reason: &'static str,
    },
    /// Image on a model without vision (400 `VISION_NOT_SUPPORTED`).
    #[error("vision not supported")]
    VisionNotSupported,
    /// Invalid/duplicate/foreign/not-ready attachment ids (400
    /// `invalid_attachment`).
    #[error("invalid attachment")]
    InvalidAttachment,
    /// Upload larger than the limit (400 `FILE_TOO_LARGE`).
    #[error("file too large")]
    FileTooLarge,
    /// Too many images in one message (400 `TOO_MANY_IMAGES`).
    #[error("too many images")]
    TooManyImages,
    /// Message exceeds `max_input_tokens` (400 `INPUT_TOO_LONG`).
    #[error("input too long")]
    InputTooLong,
    /// Mandatory context does not fit the budget (400
    /// `CONTEXT_BUDGET_EXCEEDED`).
    #[error("context budget exceeded")]
    ContextBudgetExceeded,
    /// Kill switch rejection (400 `FEATURE_DISABLED`).
    #[error("feature disabled: {0}")]
    FeatureDisabled(&'static str),
    /// Mutation of a non-terminal turn (400 `turn_state`/`STATE`).
    #[error("turn state precondition failed")]
    TurnStateNotTerminal,
    /// Reaction on a non-assistant message (400 `reaction_target`/`STATE`).
    #[error("reaction target precondition failed")]
    ReactionTargetNotAssistant,
    /// `AuthZ` denied (403 `AUTHZ_DENIED`).
    #[error("access denied")]
    PermissionDenied,
    /// PDP evaluation failure (503 + `Retry-After: 5`).
    #[error("authorization evaluation failed")]
    PdpUnavailable,
    /// Another turn is running in the chat (409 `turn_already_running`).
    #[error("turn already running")]
    TurnAlreadyRunning,
    /// `request_id` reused for a non-completed or deleted turn (409
    /// `request_id_conflict`).
    #[error("request id conflict")]
    RequestIdConflict,
    /// Mutation of a turn that is not the latest (409 `NOT_LATEST_TURN`).
    #[error("not the latest turn")]
    NotLatestTurn,
    /// Concurrent mutation lost the running-turn race (409
    /// `GENERATION_IN_PROGRESS`).
    #[error("generation in progress")]
    GenerationInProgress,
    /// Deleting an attachment referenced by a message (409
    /// `attachment_locked`).
    #[error("attachment locked")]
    AttachmentLocked,
    /// Vector store provider mismatch (409 `provider_mismatch`).
    #[error("provider mismatch")]
    ProviderMismatch,
    /// Unhandled unique-constraint violation (409 `unique_violation`).
    #[error("unique violation")]
    UniqueViolation,
    /// Quota exhausted (429; scope `tokens`/`web_search`/`code_interpreter`).
    #[error("quota exceeded: {0}")]
    QuotaExceeded(&'static str),
    /// Per-chat document count limit (429 `document_limit`).
    #[error("document limit")]
    DocumentLimit,
    /// Per-chat storage limit (429 `storage_limit`).
    #[error("storage limit")]
    StorageLimit,
    /// Storage backend failure on upload (503 + `Retry-After: 10`).
    #[error("storage backend unavailable")]
    StorageUnavailable,
    /// Upload concurrency limit (503 + `Retry-After: 5`).
    #[error("upload concurrency limit")]
    UploadConcurrency,
    /// Chat-cleanup outbox payload exceeds the outbox size limit on
    /// `DELETE /chats/{id}` (400 `invalid_argument` with `format`).
    #[error("outbox payload too large")]
    OutboxPayloadTooLarge,
    /// Internal / database error (500).
    #[error("internal error")]
    Internal,
    /// A pre-shaped canonical error (e.g. from the `OData` layer), passed
    /// through unchanged.
    #[error("{0}")]
    Canonical(#[from] CanonicalError),
}

impl DomainError {
    /// Whether this error is a unique-constraint violation reported by the
    /// persistence layer.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::UniqueViolation)
    }
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        if let Some(db_err) = db_err_source(&err)
            && toolkit_db::secure::is_unique_violation(db_err)
        {
            tracing::debug!("mini-chat: unique constraint violation: {db_err}");
            return Self::UniqueViolation;
        }
        tracing::error!("mini-chat: database error: {err}");
        Self::Internal
    }
}

impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        match err {
            ScopeError::Denied(_) => Self::PermissionDenied,
            other => {
                if let Some(db_err) = scope_err_source(&other)
                    && toolkit_db::secure::is_unique_violation(db_err)
                {
                    tracing::debug!("mini-chat: unique constraint violation: {db_err}");
                    return Self::UniqueViolation;
                }
                tracing::error!("mini-chat: scope error: {other}");
                Self::Internal
            }
        }
    }
}

fn db_err_source(err: &DbError) -> Option<&sea_orm::DbErr> {
    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(err);
    while let Some(err) = source {
        if let Some(db_err) = err.downcast_ref::<sea_orm::DbErr>() {
            return Some(db_err);
        }
        source = err.source();
    }
    None
}

fn scope_err_source(err: &ScopeError) -> Option<&sea_orm::DbErr> {
    let mut source: Option<&dyn std::error::Error> = std::error::Error::source(err);
    while let Some(err) = source {
        if let Some(db_err) = err.downcast_ref::<sea_orm::DbErr>() {
            return Some(db_err);
        }
        source = err.source();
    }
    None
}

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::NotFound(kind) => {
                let builder = match kind {
                    ResourceKind::Chat => ChatResource::not_found("chat not found"),
                    ResourceKind::Message => MessageResource::not_found("message not found"),
                    ResourceKind::Turn => TurnResource::not_found("turn not found"),
                    ResourceKind::Attachment => {
                        AttachmentResource::not_found("attachment not found")
                    }
                    ResourceKind::Model => ModelResource::not_found("model not found"),
                };
                builder.with_resource("").create()
            }
            DomainError::InvalidModel => ChatResource::invalid_argument()
                .with_field_violation("model", "unknown or disabled model", "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => ChatResource::invalid_argument()
                .with_field_violation("content", "content must be non-empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatResource::invalid_argument()
                .with_field_violation("title", "invalid title", "INVALID_TITLE")
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation("reaction", "invalid reaction", "INVALID_REACTION")
                .create(),
            DomainError::UnsupportedContentType => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "unsupported content type",
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "code interpreter unavailable",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { field, reason } => AttachmentResource::invalid_argument()
                .with_field_violation(field, "invalid multipart request", reason)
                .create(),
            DomainError::VisionNotSupported => ChatResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "the effective model does not support image input",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::InvalidAttachment => ChatResource::invalid_argument()
                .with_field_violation("attachment", "invalid attachment", "invalid_attachment")
                .create(),
            DomainError::FileTooLarge => AttachmentResource::out_of_range("file too large")
                .with_field_violation("content_length", "file too large", "FILE_TOO_LARGE")
                .create(),
            DomainError::TooManyImages => ChatResource::out_of_range("too many images")
                .with_field_violation("image_count", "too many images", "TOO_MANY_IMAGES")
                .create(),
            DomainError::InputTooLong => ChatResource::out_of_range("input too long")
                .with_field_violation("content", "input too long", "INPUT_TOO_LONG")
                .create(),
            DomainError::ContextBudgetExceeded => {
                ChatResource::out_of_range("context budget exceeded")
                    .with_field_violation(
                        "content",
                        "context budget exceeded",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }
            DomainError::FeatureDisabled(subject) => ChatResource::failed_precondition()
                .with_precondition_violation(subject, "feature disabled", "FEATURE_DISABLED")
                .create(),
            DomainError::TurnStateNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation("turn_state", "turn is not terminal", "STATE")
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "reactions are only allowed on assistant messages",
                    "STATE",
                )
                .create(),
            DomainError::PermissionDenied => ChatResource::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::PdpUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .create(),
            DomainError::TurnAlreadyRunning => TurnResource::aborted("a turn is already running")
                .with_reason("turn_already_running")
                .create(),
            DomainError::RequestIdConflict => TurnResource::aborted("request id conflict")
                .with_reason("request_id_conflict")
                .create(),
            DomainError::NotLatestTurn => TurnResource::aborted("not the latest turn")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => {
                TurnResource::aborted("generation in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::AttachmentLocked => AttachmentResource::already_exists("attachment locked")
                .with_resource("attachment_locked")
                .create(),
            DomainError::ProviderMismatch => AttachmentResource::already_exists("provider mismatch")
                .with_resource("provider_mismatch")
                .create(),
            DomainError::UniqueViolation => ChatResource::already_exists("conflict")
                .with_resource("unique_violation")
                .create(),
            DomainError::QuotaExceeded(scope) => ChatResource::resource_exhausted("quota exceeded")
                .with_quota_violation(scope, "quota_exceeded")
                .create(),
            DomainError::DocumentLimit => ChatResource::resource_exhausted("document limit")
                .with_quota_violation("document_limit", "quota_exceeded")
                .create(),
            DomainError::StorageLimit => ChatResource::resource_exhausted("storage limit")
                .with_quota_violation("storage_limit", "quota_exceeded")
                .create(),
            DomainError::StorageUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(10)
                .create(),
            DomainError::UploadConcurrency => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .create(),
            DomainError::OutboxPayloadTooLarge => ChatResource::invalid_argument()
                .with_format("outbox payload too large")
                .create(),
            DomainError::Internal => CanonicalError::internal("mini-chat internal error").create(),
            DomainError::Canonical(err) => err,
        }
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::Canonical(CanonicalError::from(err))
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(err: toolkit_db::outbox::OutboxError) -> Self {
        if matches!(err, toolkit_db::outbox::OutboxError::PayloadTooLarge { .. }) {
            return Self::OutboxPayloadTooLarge;
        }
        tracing::error!("mini-chat: outbox error: {err}");
        Self::Internal
    }
}
