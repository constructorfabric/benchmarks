//! The gear's single domain error enum.
//!
//! Mapped to canonical RFC 9457 `Problem`s in `crate::api::error` (ADR-0004). Free-form `String`
//! payloads are diagnostics for logs: the `Internal`-class variants never put them on the wire.

use thiserror::Error;
use toolkit_db::DbError;
use toolkit_db::secure::{ScopeError, is_unique_violation};

/// Domain-level failure. Not `Clone`; `Send + Sync + 'static` so it can back `DBProvider`.
#[derive(Debug, Error)]
pub enum DomainError {
    // --- not found (404) ---
    #[error("chat not found: {id}")]
    ChatNotFound { id: String },
    #[error("message not found: {id}")]
    MessageNotFound { id: String },
    #[error("turn not found: {id}")]
    TurnNotFound { id: String },
    #[error("attachment not found: {id}")]
    AttachmentNotFound { id: String },
    #[error("model not found: {id}")]
    ModelNotFound { id: String },

    // --- invalid argument (400) ---
    #[error("unknown or disabled model")]
    InvalidModel,
    #[error("content is empty")]
    EmptyContent,
    #[error("invalid chat title")]
    InvalidTitle,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    #[error("unsupported content type")]
    UnsupportedContentType,
    #[error("code interpreter unavailable for this upload")]
    CodeInterpreterUnavailable,
    #[error("multipart boundary required")]
    MultipartBoundaryRequired,
    #[error("multipart error: {0}")]
    MultipartError(String),
    #[error("multipart `file` field missing")]
    MissingFile,
    #[error("multipart `file` part has no content type")]
    MissingContentType,
    #[error("model does not support vision input")]
    VisionNotSupported,
    #[error("chat cleanup payload too large: {0}")]
    ChatCleanupPayloadTooLarge(String),

    // --- out of range (400) ---
    #[error("file too large")]
    FileTooLarge,
    #[error("too many images")]
    TooManyImages,
    #[error("input too long")]
    InputTooLong,
    #[error("context budget exceeded")]
    ContextBudgetExceeded,

    // --- failed precondition (400) ---
    /// `subject` is `web_search` or `images`.
    #[error("feature disabled: {subject}")]
    FeatureDisabled { subject: &'static str },
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reaction target is not an assistant message")]
    ReactionTargetNotAssistant,

    // --- authorization ---
    #[error("access denied")]
    AccessDenied,
    /// The PDP denied a model operation (`list_models` / `get_model`): reported with the model
    /// resource type.
    #[error("access to models denied")]
    ModelAccessDenied,
    #[error("authorization unavailable")]
    AuthzUnavailable,

    // --- aborted (409) ---
    #[error("another turn is already running")]
    TurnAlreadyRunning,
    #[error("request id conflict")]
    RequestIdConflict,
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("replay of a completed turn")]
    Replay,

    // --- already exists (409) ---
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("vector store belongs to another provider backend")]
    ProviderMismatch,
    /// Any other conflict, e.g. `unique_violation`.
    #[error("conflict: {code}")]
    Conflict { code: &'static str },

    // --- resource exhausted (429) ---
    /// `scope` is `tokens`, `web_search` or `code_interpreter`.
    #[error("quota exceeded: {scope}")]
    QuotaExceeded { scope: &'static str },
    #[error("document limit reached")]
    DocumentLimit,
    #[error("storage limit reached")]
    StorageLimit,

    // --- service unavailable (503) ---
    #[error("storage unavailable: {0}")]
    StorageUnavailable(String),
    #[error("upload concurrency limit reached")]
    UploadConcurrencyLimit,

    // --- internal (500) ---
    #[error("outbox payload too large: {0}")]
    OutboxPayloadTooLarge(String),
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("internal error: {0}")]
    Internal(String),

    /// `OData` query/pagination error, mapped by toolkit's own `From`.
    #[error(transparent)]
    OData(#[from] toolkit_odata::Error),
}

/// Classifies a raw database error: unique violations are conflicts, the rest is internal.
fn classify_db_err(err: &sea_orm::DbErr) -> DomainError {
    if is_unique_violation(err) {
        return DomainError::Conflict {
            code: "unique_violation",
        };
    }
    DomainError::Internal(format!("database error: {err}"))
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(db) => classify_db_err(&db),
            other => Self::Internal(format!("database error: {other}")),
        }
    }
}

/// Maps a secure-ORM scope error: unique violation to `Conflict`, scope denials to
/// `AccessDenied`, everything else to `Internal`.
#[must_use]
pub fn map_scope_err(err: ScopeError) -> DomainError {
    match err {
        ScopeError::Db(db) => classify_db_err(&db),
        ScopeError::TenantNotInScope { .. } | ScopeError::Denied(_) => DomainError::AccessDenied,
        other => DomainError::Internal(format!("scope error: {other}")),
    }
}
