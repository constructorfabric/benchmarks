//! Domain errors and their canonical `Problem` mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

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

/// Quota scope reported in `violations[0].subject` of a 429.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    Tokens,
    WebSearch,
    CodeInterpreter,
}

impl QuotaScope {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

/// Every error the domain can return to a REST caller.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("chat not found")]
    ChatNotFound(String),
    #[error("message not found")]
    MessageNotFound(String),
    #[error("turn not found")]
    TurnNotFound(String),
    #[error("attachment not found")]
    AttachmentNotFound(String),
    #[error("model not found")]
    ModelNotFound(String),

    #[error("invalid model: {0}")]
    InvalidModel(String),
    #[error("invalid title: {0}")]
    InvalidTitle(String),
    #[error("content must not be empty")]
    EmptyContent,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    /// Generic `invalid_argument` with one field violation on the chat resource.
    #[error("{detail}")]
    InvalidArgument {
        field: &'static str,
        reason: &'static str,
        detail: String,
    },
    #[error("code interpreter unavailable")]
    CodeInterpreterUnavailable,
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("vision not supported by model {0}")]
    VisionNotSupported(String),
    #[error("chat cleanup payload too large: {0}")]
    ChatCleanupPayloadTooLarge(String),

    #[error("file too large: {0}")]
    FileTooLarge(String),
    #[error("too many images: {0}")]
    TooManyImages(String),
    #[error("input too long: {0}")]
    InputTooLong(String),
    #[error("context budget exceeded: {0}")]
    ContextBudgetExceeded(String),

    #[error("feature disabled: {0}")]
    FeatureDisabled(&'static str),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are allowed on assistant messages only")]
    ReactionTarget,

    #[error("access denied")]
    AuthzDenied,
    #[error("authorization evaluation failed")]
    AuthzUnavailable,

    #[error("another turn is running in this chat")]
    TurnAlreadyRunning,
    #[error("request_id conflict: {0}")]
    RequestIdConflict(String),
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("completed turn replay")]
    Replay,

    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("vector store belongs to another provider backend")]
    ProviderMismatch,
    #[error("unique constraint violation: {0}")]
    UniqueViolation(String),

    #[error("quota exceeded ({})", .0.as_str())]
    QuotaExceeded(QuotaScope),
    #[error("per-chat document limit reached")]
    DocumentLimit,
    #[error("per-chat storage limit reached")]
    StorageLimit,

    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),
    #[error("too many concurrent uploads")]
    UploadConcurrency,

    #[error("internal error: {0}")]
    Internal(String),
    #[error("database error: {0}")]
    Db(#[source] sea_orm::DbErr),
    #[error("odata: {0}")]
    OData(toolkit_odata::Error),
    #[error("message persistence failed: {0}")]
    MessagePersistence(String),
}

impl DomainError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::Internal(e.to_string())
    }

    #[must_use]
    pub fn invalid(field: &'static str, reason: &'static str, detail: impl Into<String>) -> Self {
        Self::InvalidArgument {
            field,
            reason,
            detail: detail.into(),
        }
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Internal(format!("database: {e}"))
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        match e {
            toolkit_db::secure::ScopeError::Db(db) => db.into(),
            other => Self::Internal(format!("database scope: {other}")),
        }
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        Self::Db(e)
    }
}

impl DomainError {
    /// Whether this is a unique-constraint violation reported by the database.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        match self {
            Self::UniqueViolation(_) => true,
            Self::Db(e) => toolkit_db::secure::is_unique_violation(e),
            _ => false,
        }
    }

    /// The wrapped database error (used by transaction retry classification).
    #[must_use]
    pub fn db_err(&self) -> Option<&sea_orm::DbErr> {
        match self {
            Self::Db(e) => Some(e),
            _ => None,
        }
    }
}

// reason: one flat arm per domain error variant (wire mapping table)
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::ChatNotFound(id) => ChatResource::not_found("Chat not found")
                .with_resource(id)
                .create(),
            DomainError::MessageNotFound(id) => MessageResource::not_found("Message not found")
                .with_resource(id)
                .create(),
            DomainError::TurnNotFound(id) => TurnResource::not_found("Turn not found")
                .with_resource(id)
                .create(),
            DomainError::AttachmentNotFound(id) => {
                AttachmentResource::not_found("Attachment not found")
                    .with_resource(id)
                    .create()
            }
            DomainError::ModelNotFound(id) => ModelResource::not_found("Model not found")
                .with_resource(id)
                .create(),
            DomainError::InvalidModel(detail) => ChatResource::invalid_argument()
                .with_field_violation("model", detail, "INVALID_MODEL")
                .create(),
            DomainError::InvalidTitle(detail) => ChatResource::invalid_argument()
                .with_field_violation("title", detail, "INVALID_TITLE")
                .create(),
            DomainError::EmptyContent => ChatResource::invalid_argument()
                .with_field_violation("content", "content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation(
                    "reaction",
                    "reaction must be 'like' or 'dislike'",
                    "INVALID_REACTION",
                )
                .create(),
            DomainError::InvalidAttachment(detail) => ChatResource::invalid_argument()
                .with_field_violation("attachment", detail, "invalid_attachment")
                .create(),
            DomainError::InvalidArgument {
                field,
                reason,
                detail,
            } => ChatResource::invalid_argument()
                .with_field_violation(field, detail, reason)
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "code interpreter is not available for this file type",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::UnsupportedContentType(ct) => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    format!("unsupported content type: {ct}"),
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::VisionNotSupported(model) => ChatResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    format!("model {model} does not support image input"),
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::ChatCleanupPayloadTooLarge(detail) => {
                ChatResource::invalid_argument().with_format(detail).create()
            }
            DomainError::FileTooLarge(detail) => AttachmentResource::out_of_range(detail.clone())
                .with_field_violation("content_length", detail, "FILE_TOO_LARGE")
                .create(),
            DomainError::TooManyImages(detail) => ChatResource::out_of_range(detail.clone())
                .with_field_violation("image_count", detail, "TOO_MANY_IMAGES")
                .create(),
            DomainError::InputTooLong(detail) => ChatResource::out_of_range(detail.clone())
                .with_field_violation("content", detail, "INPUT_TOO_LONG")
                .create(),
            DomainError::ContextBudgetExceeded(detail) => {
                ChatResource::out_of_range(detail.clone())
                    .with_field_violation("content", detail, "CONTEXT_BUDGET_EXCEEDED")
                    .create()
            }
            DomainError::FeatureDisabled(subject) => ChatResource::failed_precondition()
                .with_precondition_violation(
                    subject,
                    format!("{subject} is disabled"),
                    "FEATURE_DISABLED",
                )
                .create(),
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "the turn is still running",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTarget => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "only assistant messages can receive reactions",
                    "STATE",
                )
                .create(),
            DomainError::AuthzDenied => ChatResource::permission_denied()
                .with_reason("AUTHZ_DENIED")
                .create(),
            DomainError::AuthzUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .create(),
            DomainError::TurnAlreadyRunning => {
                ChatResource::aborted("A generation is already running for this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict(diag) => {
                tracing::debug!(diag = %diag, "request_id conflict");
                ChatResource::aborted("The request_id is already used by another turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => {
                TurnResource::aborted("Only the latest turn can be modified")
                    .with_reason("NOT_LATEST_TURN")
                    .create()
            }
            DomainError::GenerationInProgress => {
                TurnResource::aborted("Another generation is in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResource::aborted("Completed turn replay")
                .with_reason("REPLAY")
                .create(),
            DomainError::AttachmentLocked => {
                AttachmentResource::already_exists("The attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentResource::already_exists(
                "The chat vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::UniqueViolation(diag) => {
                tracing::warn!(diag = %diag, "unhandled unique violation");
                ChatResource::already_exists("The resource already exists")
                    .with_resource("unique_violation")
                    .create()
            }
            DomainError::QuotaExceeded(scope) => {
                ChatResource::resource_exhausted("Quota exceeded")
                    .with_quota_violation(scope.as_str(), "quota_exceeded")
                    .create()
            }
            DomainError::DocumentLimit => {
                AttachmentResource::resource_exhausted("Per-chat document limit reached")
                    .with_quota_violation("document_limit", "document_limit")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentResource::resource_exhausted("Per-chat storage limit reached")
                    .with_quota_violation("storage_limit", "storage_limit")
                    .create()
            }
            DomainError::StorageUnavailable(diag) => {
                tracing::warn!(diag = %diag, "storage backend failure");
                CanonicalError::service_unavailable()
                    .with_retry_after_seconds(10)
                    .with_detail("Service temporarily unavailable")
                    .create()
            }
            DomainError::UploadConcurrency => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("Too many concurrent uploads")
                .create(),
            DomainError::Internal(diag) | DomainError::MessagePersistence(diag) => {
                CanonicalError::internal(diag).create()
            }
            DomainError::OData(e) => e.into(),
            DomainError::Db(e) => {
                if toolkit_db::secure::is_unique_violation(&e) {
                    tracing::warn!(diag = %e, "unhandled unique violation");
                    return ChatResource::already_exists("The resource already exists")
                        .with_resource("unique_violation")
                        .create();
                }
                CanonicalError::internal(format!("database: {e}")).create()
            }
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
