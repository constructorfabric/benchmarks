//! Domain errors. Mapped to the canonical `Problem` contract (ADR-0004) in
//! `api::rest::error`.

use toolkit_db::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotFoundKind {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("{0:?} not found")]
    NotFound(NotFoundKind),

    // ── invalid_argument (400) ──
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
    #[error("image input is not supported by the model")]
    VisionNotSupported,
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("code interpreter is unavailable for this file")]
    CodeInterpreterUnavailable,
    /// Multipart parsing failure (`field`, `reason`, detail).
    #[error("multipart error: {2}")]
    Multipart(&'static str, &'static str, String),
    /// Chat-cleanup payload too large on `DELETE /chats/{id}` (400 format).
    #[error("{0}")]
    PayloadTooLarge(String),

    // ── out_of_range (400) ──
    #[error("file too large")]
    FileTooLarge,
    #[error("too many images")]
    TooManyImages,
    #[error("message exceeds the input token limit")]
    InputTooLong,
    #[error("mandatory context does not fit the budget")]
    ContextBudgetExceeded,

    // ── failed_precondition (400) ──
    #[error("feature disabled: {0}")]
    FeatureDisabled(&'static str),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are allowed on assistant messages only")]
    ReactionTarget,

    // ── permission_denied (403) / service_unavailable (503) ──
    #[error("access denied")]
    AccessDenied,
    #[error("authorization unavailable: {0}")]
    AuthzUnavailable(String),

    // ── aborted (409) ──
    #[error("a turn is already running in this chat")]
    TurnAlreadyRunning,
    #[error("request_id conflict")]
    RequestIdConflict,
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("replay")]
    Replay,

    // ── already_exists (409) ──
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("vector store belongs to another provider backend")]
    ProviderMismatch,
    #[error("unique constraint violation")]
    UniqueViolation,

    // ── resource_exhausted (429) ──
    #[error("quota exceeded ({0})")]
    QuotaExceeded(&'static str),
    #[error("document limit reached")]
    DocumentLimit,
    #[error("storage limit reached")]
    StorageLimit,

    // ── service_unavailable (503) ──
    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),
    #[error("too many concurrent uploads")]
    UploadConcurrency,
    /// Transient database lock contention that outlived `busy_timeout`.
    #[error("database contention: {0}")]
    Contention(String),

    #[error("odata: {0}")]
    OData(toolkit_odata::Error),

    #[error("internal: {0}")]
    Internal(String),
}

impl DomainError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
}

/// Whether a `DbErr` is transient lock contention (SQLite busy / Postgres
/// serialization failure or deadlock) that is safe to retry.
fn is_contention(err: &sea_orm::DbErr) -> bool {
    use sea_orm::DbBackend;
    use toolkit_db::contention::is_retryable_contention;
    is_retryable_contention(DbBackend::Sqlite, err) || is_retryable_contention(DbBackend::Postgres, err)
}

/// Whether a `DbErr` is a unique-constraint violation.
#[must_use]
pub fn is_unique_violation(err: &sea_orm::DbErr) -> bool {
    if matches!(
        err.sql_err(),
        Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
    ) {
        return true;
    }
    let s = err.to_string();
    s.contains("UNIQUE constraint failed") || s.contains("duplicate key value")
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match &err {
            DbError::Sea(db) if is_unique_violation(db) => Self::UniqueViolation,
            DbError::Sea(db) if is_contention(db) => Self::Contention(err.to_string()),
            _ => Self::Internal(format!("database error: {err}")),
        }
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(err: sea_orm::DbErr) -> Self {
        if is_unique_violation(&err) {
            Self::UniqueViolation
        } else if is_contention(&err) {
            Self::Contention(err.to_string())
        } else {
            Self::Internal(format!("database error: {err}"))
        }
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        let s = err.to_string();
        if s.contains("UNIQUE constraint failed") || s.contains("duplicate key value") {
            Self::UniqueViolation
        } else if s.contains("database is locked") {
            Self::Contention(s)
        } else {
            Self::Internal(format!("database error: {s}"))
        }
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::OData(err)
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(err: toolkit_db::outbox::OutboxError) -> Self {
        let s = err.to_string();
        if s.to_ascii_lowercase().contains("payload") && s.to_ascii_lowercase().contains("large") {
            Self::PayloadTooLarge(s)
        } else {
            Self::Internal(format!("outbox: {s}"))
        }
    }
}
