//! Domain error type of the mini-chat gear.

use uuid::Uuid;

/// Quota scope of a `resource_exhausted` rejection.
pub mod quota_scope {
    pub const TOKENS: &str = "tokens";
    pub const WEB_SEARCH: &str = "web_search";
    pub const CODE_INTERPRETER: &str = "code_interpreter";
}

/// Domain error. Mapped to the canonical error contract (ADR-0004) in
/// `api::rest::error`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    #[error("chat not found")]
    ChatNotFound(Uuid),
    #[error("message not found")]
    MessageNotFound(Uuid),
    #[error("turn not found")]
    TurnNotFound(Uuid),
    #[error("attachment not found")]
    AttachmentNotFound(Uuid),
    #[error("model not found")]
    ModelNotFound(String),

    #[error("invalid model: {0}")]
    InvalidModel(String),
    #[error("content must not be empty")]
    EmptyContent,
    #[error("invalid title: {0}")]
    InvalidTitle(String),
    #[error("invalid reaction: {0}")]
    InvalidReaction(String),
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    #[error("unsupported content type: {content_type}")]
    UnsupportedContentType { field: &'static str, content_type: String },
    #[error("code interpreter unavailable for this file")]
    CodeInterpreterUnavailable,
    #[error("invalid multipart request: {detail}")]
    Multipart { field: &'static str, reason: &'static str, detail: String },
    #[error("model does not support image input: {0}")]
    VisionNotSupported(String),
    #[error("file too large (limit {limit_bytes} bytes)")]
    FileTooLarge { limit_bytes: u64 },
    #[error("too many images in one message ({count} > {max})")]
    TooManyImages { count: usize, max: u32 },
    #[error("message exceeds the input token limit ({estimated} > {max})")]
    InputTooLong { estimated: i64, max: u32 },
    #[error("context budget exceeded: {0}")]
    ContextBudgetExceeded(String),
    #[error("feature disabled: {0}")]
    FeatureDisabled(&'static str),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are allowed on assistant messages only")]
    ReactionTargetNotAssistant,
    #[error("chat-cleanup payload too large: {0}")]
    ChatCleanupPayloadTooLarge(String),

    #[error("permission denied")]
    Forbidden,
    #[error("authorization service unavailable")]
    AuthzUnavailable(String),

    #[error("another turn is already running in this chat")]
    TurnAlreadyRunning,
    #[error("request_id conflict: {0}")]
    RequestIdConflict(String),
    #[error("turn is not the latest turn")]
    NotLatestTurn,
    #[error("a generation is already in progress")]
    GenerationInProgress,
    #[error("completed turn replay")]
    Replay,

    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("chat vector store belongs to another provider backend")]
    ProviderMismatch,
    #[error("unique constraint violation: {0}")]
    UniqueViolation(String),

    #[error("quota exceeded ({0})")]
    QuotaExceeded(&'static str),
    #[error("per-chat document limit reached")]
    DocumentLimit,
    #[error("per-chat storage limit reached")]
    StorageLimit,

    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),
    #[error("too many concurrent uploads")]
    UploadConcurrencyLimit,

    #[error(transparent)]
    OData(#[from] toolkit_odata::Error),

    #[error("internal error: {0}")]
    Internal(String),

    /// Retryable database contention (SQLite busy / PG serialization failure).
    #[error("database contention: {0}")]
    Contention(String),
}

impl DomainError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// The error is a retryable database contention.
    #[must_use]
    pub const fn is_contention(&self) -> bool {
        matches!(self, Self::Contention(_))
    }
}

fn contention_text(s: &str) -> bool {
    s.contains("(code: 517)")
        || s.contains("(code: 5)")
        || s.contains("database is locked")
        || s.contains("could not serialize access")
        || s.contains("deadlock detected")
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        match err {
            toolkit_db::DbError::Sea(db) => classify_db_err(&db),
            other => {
                let text = other.to_string();
                if contention_text(&text) {
                    Self::Contention(text)
                } else {
                    Self::Internal(format!("database error: {text}"))
                }
            }
        }
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(err: sea_orm::DbErr) -> Self {
        classify_db_err(&err)
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        match err {
            toolkit_db::secure::ScopeError::Db(db) => classify_db_err(&db),
            toolkit_db::secure::ScopeError::TenantNotInScope { .. } => Self::Forbidden,
            other => Self::Internal(format!("scope error: {other}")),
        }
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(err: toolkit_db::outbox::OutboxError) -> Self {
        match err {
            toolkit_db::outbox::OutboxError::Database(db) => classify_db_err(&db),
            other => Self::Internal(format!("outbox error: {other}")),
        }
    }
}

/// Classify a database error (unique violation -> conflict).
#[must_use]
pub fn classify_db_err(err: &sea_orm::DbErr) -> DomainError {
    if toolkit_db::secure::is_unique_violation(err) {
        return DomainError::UniqueViolation(err.to_string());
    }
    let backend_contention = [sea_orm::DbBackend::Sqlite, sea_orm::DbBackend::Postgres]
        .into_iter()
        .any(|b| toolkit_db::contention::is_retryable_contention(b, err));
    if backend_contention || contention_text(&err.to_string()) {
        return DomainError::Contention(err.to_string());
    }
    if matches!(err, sea_orm::DbErr::RecordNotFound(_)) {
        return DomainError::Internal(format!("record not found: {err}"));
    }
    DomainError::Internal(format!("database error: {err}"))
}

/// Map a PEP error (fail-closed).
#[must_use]
pub fn map_enforcer_err(err: authz_resolver_sdk::EnforcerError) -> DomainError {
    match err {
        authz_resolver_sdk::EnforcerError::Denied { .. }
        | authz_resolver_sdk::EnforcerError::CompileFailed(_) => {
            tracing::warn!(error = %err, "authorization denied");
            DomainError::Forbidden
        }
        authz_resolver_sdk::EnforcerError::EvaluationFailed(e) => {
            tracing::error!(error = %e, "authorization evaluation failed");
            DomainError::AuthzUnavailable(e.to_string())
        }
    }
}
