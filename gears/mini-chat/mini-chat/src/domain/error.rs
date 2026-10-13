//! Domain errors. Mapped to canonical `Problem` responses in
//! `api::rest::error` (ADR-0004).

use thiserror::Error;

/// Quota scope reported in a 429 `resource_exhausted` violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    Tokens,
    WebSearch,
    CodeInterpreter,
}

impl QuotaScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

/// Feature subject of a kill-switch rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSubject {
    WebSearch,
    Images,
}

impl FeatureSubject {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::Images => "images",
        }
    }
}

/// Domain error of the mini-chat gear.
#[derive(Debug, Error)]
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
    #[error("invalid title")]
    InvalidTitle,
    #[error("content must not be empty")]
    EmptyContent,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    #[error("too many images in one message")]
    TooManyImages,
    #[error("the model does not support image input")]
    VisionNotSupported,
    #[error("message exceeds the model input limit")]
    InputTooLong,
    #[error("mandatory context does not fit the token budget")]
    ContextBudgetExceeded,
    #[error("feature disabled: {}", .0.as_str())]
    FeatureDisabled(FeatureSubject),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are allowed on assistant messages only")]
    ReactionTargetNotAssistant,

    #[error("file too large")]
    FileTooLarge,
    #[error("unsupported content type")]
    UnsupportedContentType,
    #[error("code interpreter unavailable for this file")]
    CodeInterpreterUnavailable,
    #[error("invalid multipart request: {reason}")]
    Multipart {
        field: &'static str,
        reason: &'static str,
        detail: String,
    },

    #[error("access denied")]
    AccessDenied,
    #[error("authorization service unavailable")]
    AuthzUnavailable,

    #[error("another turn is running in this chat")]
    TurnAlreadyRunning,
    #[error("request id conflict")]
    RequestIdConflict,
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("completed turn replay")]
    Replay,
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("provider mismatch")]
    ProviderMismatch,
    #[error("unique constraint violation")]
    UniqueViolation,

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

    #[error("invalid query: {0}")]
    OData(#[from] toolkit_odata::Error),
    #[error("outbox payload too large: {0}")]
    OutboxPayloadTooLarge(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// Internal error from any displayable cause.
    pub fn internal(cause: impl std::fmt::Display) -> Self {
        Self::Internal(cause.to_string())
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        if let toolkit_db::DbError::Sea(db) = &e
            && toolkit_db::secure::is_unique_violation(db)
        {
            return Self::UniqueViolation;
        }
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        if e.is_unique_violation() {
            return Self::UniqueViolation;
        }
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&e) {
            return Self::UniqueViolation;
        }
        Self::Internal(format!("database error: {e}"))
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(e: toolkit_db::outbox::OutboxError) -> Self {
        match e {
            toolkit_db::outbox::OutboxError::PayloadTooLarge { .. } => {
                Self::OutboxPayloadTooLarge(e.to_string())
            }
            other => Self::Internal(format!("outbox error: {other}")),
        }
    }
}

impl From<serde_json::Error> for DomainError {
    fn from(e: serde_json::Error) -> Self {
        Self::Internal(format!("serialization error: {e}"))
    }
}
