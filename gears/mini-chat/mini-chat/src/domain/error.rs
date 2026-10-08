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

/// Quota scopes reported in `resource_exhausted` violations.
pub mod quota_scope {
    pub const TOKENS: &str = "tokens";
    pub const WEB_SEARCH: &str = "web_search";
    pub const CODE_INTERPRETER: &str = "code_interpreter";
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("chat not found")]
    ChatNotFound,
    #[error("message not found")]
    MessageNotFound,
    #[error("turn not found")]
    TurnNotFound,
    #[error("attachment not found")]
    AttachmentNotFound,
    #[error("model not found")]
    ModelNotFound,
    #[error("invalid model")]
    InvalidModel,
    #[error("content must not be empty")]
    EmptyContent,
    #[error("invalid title")]
    InvalidTitle,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    #[error("file too large: {0}")]
    FileTooLarge(String),
    #[error("too many images")]
    TooManyImages,
    #[error("input too long")]
    InputTooLong,
    #[error("context budget exceeded")]
    ContextBudgetExceeded,
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("code interpreter unavailable")]
    CodeInterpreterUnavailable,
    #[error("multipart error: {detail}")]
    Multipart {
        field: &'static str,
        reason: &'static str,
        detail: String,
    },
    #[error("vision not supported")]
    VisionNotSupported,
    #[error("feature disabled: {0}")]
    FeatureDisabled(&'static str),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reaction target is not an assistant message")]
    ReactionTargetNotAssistant,
    #[error("access denied")]
    AuthzDenied,
    #[error("authorization unavailable")]
    AuthzUnavailable,
    #[error("a turn is already running")]
    TurnAlreadyRunning,
    #[error("request id conflict")]
    RequestIdConflict,
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("replay")]
    Replay,
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("vector store provider mismatch")]
    ProviderMismatch,
    #[error("unique violation")]
    UniqueViolation,
    #[error("quota exceeded: {0}")]
    QuotaExceeded(&'static str),
    #[error("document limit reached")]
    DocumentLimit,
    #[error("storage limit reached")]
    StorageLimit,
    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),
    #[error("too many concurrent uploads")]
    UploadConcurrency,
    #[error("outbox payload too large: {0}")]
    OutboxPayloadTooLarge(String),
    #[error(transparent)]
    OData(#[from] toolkit_odata::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        Self::Internal(format!("database: {e}"))
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        if e.is_unique_violation() {
            return Self::UniqueViolation;
        }
        Self::Internal(format!("database: {e}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&e) {
            return Self::UniqueViolation;
        }
        Self::Internal(format!("database: {e}"))
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(e: toolkit_db::outbox::OutboxError) -> Self {
        match e {
            toolkit_db::outbox::OutboxError::PayloadTooLarge { size, max } => {
                Self::OutboxPayloadTooLarge(format!("outbox payload of {size} bytes exceeds {max} bytes"))
            }
            other => Self::Internal(format!("outbox: {other}")),
        }
    }
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines)]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::ChatNotFound => ChatResource::not_found("Chat not found").with_resource("chat").create(),
            DomainError::MessageNotFound => {
                MessageResource::not_found("Message not found").with_resource("message").create()
            }
            DomainError::TurnNotFound => TurnResource::not_found("Turn not found").with_resource("turn").create(),
            DomainError::AttachmentNotFound => AttachmentResource::not_found("Attachment not found")
                .with_resource("attachment")
                .create(),
            DomainError::ModelNotFound => ModelResource::not_found("Model not found").with_resource("model").create(),
            DomainError::InvalidModel => ChatResource::invalid_argument()
                .with_field_violation("model", "model is not available in the catalog", "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => ChatResource::invalid_argument()
                .with_field_violation("content", "content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatResource::invalid_argument()
                .with_field_violation("title", "title must be 1-255 characters after trimming", "INVALID_TITLE")
                .create(),
            DomainError::InvalidReaction => MessageResource::invalid_argument()
                .with_field_violation("reaction", "reaction must be 'like' or 'dislike'", "INVALID_REACTION")
                .create(),
            DomainError::InvalidAttachment(d) => ChatResource::invalid_argument()
                .with_field_violation("attachment", d, "invalid_attachment")
                .create(),
            DomainError::FileTooLarge(d) => AttachmentResource::out_of_range("File too large")
                .with_field_violation("content_length", d, "FILE_TOO_LARGE")
                .create(),
            DomainError::TooManyImages => ChatResource::out_of_range("Too many images")
                .with_field_violation("image_count", "too many images in one message", "TOO_MANY_IMAGES")
                .create(),
            DomainError::InputTooLong => ChatResource::out_of_range("Input too long")
                .with_field_violation("content", "message exceeds the model input limit", "INPUT_TOO_LONG")
                .create(),
            DomainError::ContextBudgetExceeded => ChatResource::out_of_range("Context budget exceeded")
                .with_field_violation(
                    "content",
                    "mandatory context does not fit the model context window",
                    "CONTEXT_BUDGET_EXCEEDED",
                )
                .create(),
            DomainError::UnsupportedContentType(d) => AttachmentResource::invalid_argument()
                .with_field_violation("content_type", d, "UNSUPPORTED_CONTENT_TYPE")
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "code interpreter is not available for this chat",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { field, reason, detail } => AttachmentResource::invalid_argument()
                .with_field_violation(field, detail, reason)
                .create(),
            DomainError::VisionNotSupported => ChatResource::invalid_argument()
                .with_field_violation("content_type", "the model does not support image input", "VISION_NOT_SUPPORTED")
                .create(),
            DomainError::FeatureDisabled(subject) => ChatResource::failed_precondition()
                .with_precondition_violation(subject, format!("{subject} is disabled"), "FEATURE_DISABLED")
                .create(),
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation("turn_state", "turn is not in a terminal state", "STATE")
                .create(),
            DomainError::ReactionTargetNotAssistant => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "reactions are allowed on assistant messages only",
                    "STATE",
                )
                .create(),
            DomainError::AuthzDenied => ChatResource::permission_denied().with_reason("AUTHZ_DENIED").create(),
            DomainError::AuthzUnavailable => CanonicalError::service_unavailable()
                .with_detail("Service temporarily unavailable")
                .with_retry_after_seconds(5)
                .create(),
            DomainError::TurnAlreadyRunning => ChatResource::aborted("A turn is already running in this chat")
                .with_reason("turn_already_running")
                .create(),
            DomainError::RequestIdConflict => TurnResource::aborted("The request id is already used")
                .with_reason("request_id_conflict")
                .create(),
            DomainError::NotLatestTurn => TurnResource::aborted("Only the latest turn can be modified")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => TurnResource::aborted("A generation is already in progress")
                .with_reason("GENERATION_IN_PROGRESS")
                .create(),
            DomainError::Replay => TurnResource::aborted("Completed turn replay").with_reason("REPLAY").create(),
            DomainError::AttachmentLocked => AttachmentResource::already_exists("Attachment is referenced by a message")
                .with_resource("attachment_locked")
                .create(),
            DomainError::ProviderMismatch => AttachmentResource::already_exists(
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
            DomainError::DocumentLimit => AttachmentResource::resource_exhausted("Document limit reached")
                .with_quota_violation("document_limit", "per-chat document limit reached")
                .create(),
            DomainError::StorageLimit => AttachmentResource::resource_exhausted("Storage limit reached")
                .with_quota_violation("storage_limit", "per-chat storage limit reached")
                .create(),
            DomainError::StorageUnavailable(cause) => {
                tracing::warn!(cause = %cause, "storage backend unavailable");
                CanonicalError::service_unavailable()
                    .with_detail("Service temporarily unavailable")
                    .with_retry_after_seconds(10)
                    .create()
            }
            DomainError::UploadConcurrency => CanonicalError::service_unavailable()
                .with_detail("Too many concurrent uploads")
                .with_retry_after_seconds(5)
                .create(),
            DomainError::OutboxPayloadTooLarge(d) => ChatResource::invalid_argument().with_format(d).create(),
            DomainError::OData(e) => CanonicalError::from(e),
            DomainError::Internal(d) => CanonicalError::internal(d).create(),
        }
    }
}
