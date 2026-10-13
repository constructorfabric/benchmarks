//! Domain error type shared by every service and repository.
//!
//! Each variant maps to exactly one canonical REST error (see
//! `api::rest::error`); the `Display` text is for operators and logs, never
//! for the wire.

use thiserror::Error;

use crate::domain::model::QuotaScope;

/// Result alias used across the domain layer.
pub type DomainResult<T> = Result<T, DomainError>;

/// Conflict code reported for a unique-constraint violation raised by the DB layer.
pub const UNIQUE_VIOLATION: &str = "unique_violation";

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

    #[error("unknown, disabled or unavailable model")]
    InvalidModel,
    #[error("message content is empty")]
    EmptyContent,
    #[error("invalid chat title")]
    InvalidTitle,
    #[error("invalid reaction value")]
    InvalidReaction,
    #[error("invalid attachment ids: {0}")]
    InvalidAttachment(String),
    #[error("too many images (max {max})")]
    TooManyImages { max: u32 },
    #[error("input exceeds max_input_tokens")]
    InputTooLong,
    #[error("mandatory context does not fit the token budget")]
    ContextBudgetExceeded,
    #[error("model does not support vision")]
    VisionNotSupported,
    #[error("file exceeds {limit_bytes} bytes")]
    FileTooLarge { limit_bytes: u64 },
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("code interpreter is unavailable")]
    CodeInterpreterUnavailable,
    #[error("multipart error on `{field}` ({reason}): {detail}")]
    Multipart {
        field: &'static str,
        reason: &'static str,
        detail: String,
    },

    #[error("feature disabled: {subject}")]
    FeatureDisabled { subject: &'static str },
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reaction target is not an assistant message")]
    ReactionTargetNotAssistant,

    #[error("permission denied")]
    PermissionDenied,
    #[error("authorization service unavailable")]
    AuthzUnavailable,

    #[error("another turn is already running in this chat")]
    TurnAlreadyRunning,
    #[error("request_id conflict")]
    RequestIdConflict,
    #[error("turn is not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("replay of a completed turn")]
    Replay,

    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("vector store was created for another provider backend")]
    ProviderMismatch,
    #[error("conflict: {code}")]
    Conflict { code: String },

    #[error("quota exceeded: {scope}")]
    QuotaExceeded { scope: QuotaScope },
    #[error("per-chat document limit reached")]
    DocumentLimit,
    #[error("per-chat storage limit reached")]
    StorageLimit,
    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),
    #[error("upload concurrency limit reached")]
    UploadConcurrencyLimit,

    #[error("outbox payload too large: {0}")]
    OutboxPayloadTooLarge(String),

    /// Transient database contention (`SQLite` `SQLITE_BUSY` /
    /// `SQLITE_BUSY_SNAPSHOT`, `PostgreSQL` serialization failure / deadlock):
    /// the transaction was rolled back and may be re-run as a whole (see
    /// `infra::db::tx::with_tx_retry`). Only produced from database errors.
    #[error("database contention: {0}")]
    DbContention(String),

    #[error("internal error: {0}")]
    Internal(String),
}

impl DomainError {
    /// Internal error from anything displayable.
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        Self::from(toolkit_db::DbError::from(err))
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        use toolkit_db::DbError;
        use toolkit_db::secure::{ScopeError, is_unique_violation};

        let unique = match &err {
            DbError::Sea(e) => is_unique_violation(e),
            DbError::Sqlx(e) => e
                .as_database_error()
                .is_some_and(sea_orm::sqlx::error::DatabaseError::is_unique_violation),
            DbError::Other(e) => e
                .downcast_ref::<ScopeError>()
                .is_some_and(ScopeError::is_unique_violation),
            _ => false,
        };
        if unique {
            Self::Conflict {
                code: UNIQUE_VIOLATION.to_owned(),
            }
        } else if is_db_contention(&sea_orm::DbErr::Custom(err.to_string())) {
            Self::DbContention(err.to_string())
        } else {
            Self::Internal(err.to_string())
        }
    }
}

/// Whether `err` is transient lock contention on one of the gear's backends
/// (`SQLite`, `PostgreSQL`), per the toolkit classifier. The signatures of the
/// two engines are disjoint, so checking both needs no backend.
pub(crate) fn is_db_contention(err: &sea_orm::DbErr) -> bool {
    use sea_orm::DbBackend;
    use toolkit_db::contention::is_retryable_contention;

    is_retryable_contention(DbBackend::Sqlite, err)
        || is_retryable_contention(DbBackend::Postgres, err)
}
