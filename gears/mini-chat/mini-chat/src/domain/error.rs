//! Domain errors and their canonical REST mapping (ADR-0004).

use toolkit_canonical_errors::{CanonicalError, resource_error};

/// `gts.cf.core.mini_chat.chat.v1~`
#[resource_error(gts_id!("cf.core.mini_chat.chat.v1~"))]
pub struct ChatResource;
/// `gts.cf.core.mini_chat.message.v1~`
#[resource_error(gts_id!("cf.core.mini_chat.message.v1~"))]
pub struct MessageResource;
/// `gts.cf.core.mini_chat.turn.v1~`
#[resource_error(gts_id!("cf.core.mini_chat.turn.v1~"))]
pub struct TurnResource;
/// `gts.cf.core.mini_chat.attachment.v1~`
#[resource_error(gts_id!("cf.core.mini_chat.attachment.v1~"))]
pub struct AttachmentResource;
/// `gts.cf.core.mini_chat.model.v1~`
#[resource_error(gts_id!("cf.core.mini_chat.model.v1~"))]
pub struct ModelResource;
/// `gts.cf.core.mini_chat.quota.v1~`
#[resource_error(gts_id!("cf.core.mini_chat.quota.v1~"))]
pub struct QuotaResource;

/// Quota scope of a 429 rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    /// Credit quota.
    Tokens,
    /// Daily web search quota.
    WebSearch,
    /// Daily code interpreter quota.
    CodeInterpreter,
}

impl QuotaScope {
    /// Wire subject.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

/// Every error the gear returns before a stream is opened.
#[derive(Debug, Clone, thiserror::Error)]
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
    #[error("empty content")]
    EmptyContent,
    #[error("invalid title")]
    InvalidTitle,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    #[error("unsupported content type {0}")]
    UnsupportedContentType(String),
    #[error("code interpreter unavailable")]
    CodeInterpreterUnavailable,
    #[error("invalid multipart request: {reason}")]
    Multipart { field: &'static str, reason: &'static str, description: String },
    #[error("vision not supported")]
    VisionNotSupported,
    #[error("file too large")]
    FileTooLarge { limit_bytes: u64 },
    #[error("too many images")]
    TooManyImages { limit: u32 },
    #[error("input too long")]
    InputTooLong,
    #[error("context budget exceeded")]
    ContextBudgetExceeded,
    #[error("feature disabled: {0}")]
    FeatureDisabled(&'static str),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reaction target is not an assistant message")]
    ReactionTarget,
    #[error("permission denied")]
    PermissionDenied,
    #[error("authorization service unavailable")]
    PdpUnavailable,
    #[error("a turn is already running")]
    TurnAlreadyRunning,
    #[error("request id conflict: {0}")]
    RequestIdConflict(String),
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("replay of a completed turn")]
    Replay,
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("chat vector store belongs to another provider backend")]
    ProviderMismatch,
    #[error("conflict: {0}")]
    Conflict(String),
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
    #[error("chat cleanup payload too large: {0}")]
    CleanupPayloadTooLarge(String),
    #[error("odata: {0}")]
    OData(String),
    #[error("internal: {0}")]
    Internal(String),
    #[error("{0}")]
    Canonical(Box<CanonicalError>),
}

impl DomainError {
    /// Internal error helper.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
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
            return Self::Conflict("unique_violation".to_owned());
        }
        Self::Internal(format!("database: {e}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&e) {
            return Self::Conflict("unique_violation".to_owned());
        }
        Self::Internal(format!("database: {e}"))
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(e: toolkit_odata::Error) -> Self {
        Self::Canonical(Box::new(CanonicalError::from(e)))
    }
}

impl From<CanonicalError> for DomainError {
    fn from(e: CanonicalError) -> Self {
        Self::Canonical(Box::new(e))
    }
}

impl From<DomainError> for CanonicalError {
    #[allow(clippy::too_many_lines)]
    fn from(err: DomainError) -> Self {
        match err {
            DomainError::ChatNotFound => {
                ChatResource::not_found("Chat not found").with_resource("chat").create()
            }
            DomainError::MessageNotFound => {
                MessageResource::not_found("Message not found").with_resource("message").create()
            }
            DomainError::TurnNotFound => {
                TurnResource::not_found("Turn not found").with_resource("turn").create()
            }
            DomainError::AttachmentNotFound => AttachmentResource::not_found("Attachment not found")
                .with_resource("attachment")
                .create(),
            DomainError::ModelNotFound => {
                ModelResource::not_found("Model not found").with_resource("model").create()
            }
            DomainError::InvalidModel => ChatResource::invalid_argument()
                .with_field_violation("model", "Model is not available in the catalog", "INVALID_MODEL")
                .create(),
            DomainError::EmptyContent => MessageResource::invalid_argument()
                .with_field_violation("content", "Content must not be empty", "EMPTY_CONTENT")
                .create(),
            DomainError::InvalidTitle => ChatResource::invalid_argument()
                .with_field_violation(
                    "title",
                    "Title must be 1-255 characters after trimming",
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
            DomainError::InvalidAttachment(d) => MessageResource::invalid_argument()
                .with_field_violation("attachment", d, "invalid_attachment")
                .create(),
            DomainError::UnsupportedContentType(ct) => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    format!("Unsupported content type: {ct}"),
                    "UNSUPPORTED_CONTENT_TYPE",
                )
                .create(),
            DomainError::CodeInterpreterUnavailable => AttachmentResource::invalid_argument()
                .with_field_violation(
                    "file",
                    "Code interpreter is not available for this chat",
                    "CODE_INTERPRETER_UNAVAILABLE",
                )
                .create(),
            DomainError::Multipart { field, reason, description } => {
                AttachmentResource::invalid_argument()
                    .with_field_violation(field, description, reason)
                    .create()
            }
            DomainError::VisionNotSupported => MessageResource::invalid_argument()
                .with_field_violation(
                    "content_type",
                    "The model does not support image input",
                    "VISION_NOT_SUPPORTED",
                )
                .create(),
            DomainError::FileTooLarge { limit_bytes } => {
                AttachmentResource::out_of_range("File is too large")
                    .with_field_violation(
                        "content_length",
                        format!("File exceeds the limit of {limit_bytes} bytes"),
                        "FILE_TOO_LARGE",
                    )
                    .create()
            }
            DomainError::TooManyImages { limit } => {
                MessageResource::out_of_range("Too many images")
                    .with_field_violation(
                        "image_count",
                        format!("At most {limit} images per message"),
                        "TOO_MANY_IMAGES",
                    )
                    .create()
            }
            DomainError::InputTooLong => MessageResource::out_of_range("Message is too long")
                .with_field_violation(
                    "content",
                    "Message exceeds the model input limit",
                    "INPUT_TOO_LONG",
                )
                .create(),
            DomainError::ContextBudgetExceeded => {
                MessageResource::out_of_range("Context budget exceeded")
                    .with_field_violation(
                        "content",
                        "Mandatory context does not fit the model input budget",
                        "CONTEXT_BUDGET_EXCEEDED",
                    )
                    .create()
            }
            DomainError::FeatureDisabled(subject) => MessageResource::failed_precondition()
                .with_precondition_violation(
                    subject,
                    format!("{subject} is disabled"),
                    "FEATURE_DISABLED",
                )
                .create(),
            DomainError::TurnNotTerminal => TurnResource::failed_precondition()
                .with_precondition_violation(
                    "turn_state",
                    "The turn is still running",
                    "STATE",
                )
                .create(),
            DomainError::ReactionTarget => MessageResource::failed_precondition()
                .with_precondition_violation(
                    "reaction_target",
                    "Only assistant messages can receive reactions",
                    "STATE",
                )
                .create(),
            DomainError::PermissionDenied => {
                ChatResource::permission_denied().with_reason("AUTHZ_DENIED").create()
            }
            DomainError::PdpUnavailable => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("Authorization service temporarily unavailable")
                .create(),
            DomainError::TurnAlreadyRunning => {
                ChatResource::aborted("A turn is already running in this chat")
                    .with_reason("turn_already_running")
                    .create()
            }
            DomainError::RequestIdConflict(_) => {
                TurnResource::aborted("The request id is already used by another turn")
                    .with_reason("request_id_conflict")
                    .create()
            }
            DomainError::NotLatestTurn => TurnResource::aborted("Only the latest turn can be changed")
                .with_reason("NOT_LATEST_TURN")
                .create(),
            DomainError::GenerationInProgress => {
                TurnResource::aborted("Another generation is in progress")
                    .with_reason("GENERATION_IN_PROGRESS")
                    .create()
            }
            DomainError::Replay => TurnResource::aborted("The turn is already completed")
                .with_reason("REPLAY")
                .create(),
            DomainError::AttachmentLocked => {
                AttachmentResource::already_exists("Attachment is referenced by a message")
                    .with_resource("attachment_locked")
                    .create()
            }
            DomainError::ProviderMismatch => AttachmentResource::already_exists(
                "The chat vector store belongs to another provider backend",
            )
            .with_resource("provider_mismatch")
            .create(),
            DomainError::Conflict(code) => {
                ChatResource::already_exists("Conflicting resource state").with_resource(code).create()
            }
            DomainError::QuotaExceeded(scope) => QuotaResource::resource_exhausted("Quota exceeded")
                .with_quota_violation(scope.as_str(), "quota_exceeded")
                .create(),
            DomainError::DocumentLimit => {
                AttachmentResource::resource_exhausted("Per-chat document limit reached")
                    .with_quota_violation("document_limit", "Per-chat document limit reached")
                    .create()
            }
            DomainError::StorageLimit => {
                AttachmentResource::resource_exhausted("Per-chat storage limit reached")
                    .with_quota_violation("storage_limit", "Per-chat storage limit reached")
                    .create()
            }
            DomainError::StorageUnavailable(_) => CanonicalError::service_unavailable()
                .with_retry_after_seconds(10)
                .with_detail("Service temporarily unavailable")
                .create(),
            DomainError::UploadConcurrency => CanonicalError::service_unavailable()
                .with_retry_after_seconds(5)
                .with_detail("Too many concurrent uploads")
                .create(),
            DomainError::CleanupPayloadTooLarge(msg) => {
                ChatResource::invalid_argument().with_format(msg).create()
            }
            DomainError::OData(msg) => CanonicalError::internal(msg).create(),
            DomainError::Internal(msg) => {
                tracing::error!(error = %msg, "mini-chat internal error");
                CanonicalError::internal(msg).create()
            }
            DomainError::Canonical(e) => *e,
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod error_tests;
