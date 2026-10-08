//! Domain errors. Mapped to the canonical error contract in `api::rest::error` (ADR-0004).

use toolkit_macros::domain_model;

/// Resource named by a `not_found` error.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

/// Subject of a `FEATURE_DISABLED` kill-switch rejection.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledFeature {
    WebSearch,
    Images,
}

impl DisabledFeature {
    #[must_use]
    pub fn subject(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::Images => "images",
        }
    }
}

/// Quota scope of a 429 `quota_exceeded`.
#[domain_model]
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

/// Domain error of every mini-chat operation.
#[domain_model]
#[derive(Debug, Clone, thiserror::Error)]
pub enum DomainError {
    #[error("{0:?} not found")]
    NotFound(Resource),
    #[error("invalid model: {0}")]
    InvalidModel(String),
    #[error("content must not be empty")]
    EmptyContent,
    #[error("title must be 1-255 characters after trimming")]
    InvalidTitle,
    #[error("reaction must be 'like' or 'dislike'")]
    InvalidReaction,
    #[error("invalid attachment: {0}")]
    InvalidAttachment(String),
    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),
    #[error("code interpreter is unavailable for this file")]
    CodeInterpreterUnavailable,
    #[error("invalid multipart request: {detail}")]
    Multipart {
        field: &'static str,
        reason: &'static str,
        detail: String,
    },
    #[error("the model does not support image input")]
    VisionNotSupported,
    #[error("file too large: limit {limit_bytes} bytes")]
    FileTooLarge { limit_bytes: u64 },
    #[error("too many images: at most {max} per message")]
    TooManyImages { max: u32 },
    #[error("message exceeds the model input limit")]
    InputTooLong,
    #[error("mandatory context does not fit the model budget")]
    ContextBudgetExceeded,
    #[error("feature disabled: {}", .0.subject())]
    FeatureDisabled(DisabledFeature),
    #[error("turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are allowed on assistant messages only")]
    ReactionTarget,
    #[error("access denied")]
    AuthzDenied,
    #[error("authorization service unavailable")]
    AuthzUnavailable,
    #[error("another turn is running in this chat")]
    TurnAlreadyRunning,
    #[error("request_id conflict: {0}")]
    RequestIdConflict(String),
    #[error("the turn is not the latest turn")]
    NotLatestTurn,
    #[error("a generation is already in progress")]
    GenerationInProgress,
    #[error("completed turn replay")]
    Replay,
    #[error("attachment is referenced by a message")]
    AttachmentLocked,
    #[error("chat vector store belongs to another provider backend")]
    ProviderMismatch,
    #[error("unique constraint violation")]
    UniqueViolation,
    #[error("quota exceeded: {}", .0.as_str())]
    QuotaExceeded(QuotaScope),
    #[error("per-chat document limit reached")]
    DocumentLimit,
    #[error("per-chat storage limit reached")]
    StorageLimit,
    #[error("storage backend unavailable: {0}")]
    StorageUnavailable(String),
    #[error("too many concurrent uploads")]
    UploadConcurrency,
    #[error("outbox payload too large: {0}")]
    PayloadTooLarge(String),
    #[error("database contention: {0}")]
    Contention(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// `true` for SQLite busy / PostgreSQL serialization or deadlock errors.
#[must_use]
pub fn is_contention_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("database is locked")
        || m.contains("database table is locked")
        || m.contains("sqlite_busy")
        || m.contains("(code: 5)")
        || m.contains("(code: 517)")
        || m.contains("could not serialize access")
        || m.contains("deadlock detected")
        || m.contains("40001")
        || m.contains("40p01")
}

fn db_error(msg: String) -> DomainError {
    if is_contention_message(&msg) {
        DomainError::Contention(msg)
    } else {
        DomainError::Internal(format!("database error: {msg}"))
    }
}

/// Retries `f` on database contention (up to 6 attempts with backoff).
///
/// # Errors
/// The last error of `f`.
pub async fn retry_contention<T, F, Fut>(mut f: F) -> DomainResult<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = DomainResult<T>>,
{
    let mut attempt = 0_u32;
    loop {
        match f().await {
            Err(DomainError::Contention(m)) if attempt < 5 => {
                attempt += 1;
                tracing::debug!(attempt, error = %m, "retrying transaction after contention");
                let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 20;
                tokio::time::sleep(std::time::Duration::from_millis(
                    20 * u64::from(attempt) + jitter,
                ))
                .await;
            }
            other => return other,
        }
    }
}

impl DomainError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// `true` for a unique violation surfaced as [`DomainError::UniqueViolation`].
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::UniqueViolation)
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        if let toolkit_db::DbError::Sea(ref db) = e
            && toolkit_db::secure::is_unique_violation(db)
        {
            return Self::UniqueViolation;
        }
        db_error(e.to_string())
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        if e.is_unique_violation() {
            return Self::UniqueViolation;
        }
        db_error(e.to_string())
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&e) {
            return Self::UniqueViolation;
        }
        db_error(e.to_string())
    }
}

impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(e: authz_resolver_sdk::EnforcerError) -> Self {
        match e {
            authz_resolver_sdk::EnforcerError::EvaluationFailed(err) => {
                tracing::error!(error = %err, "PDP evaluation failed");
                Self::AuthzUnavailable
            }
            other => {
                tracing::warn!(error = %other, "authorization denied");
                Self::AuthzDenied
            }
        }
    }
}

impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(e: toolkit_db::outbox::OutboxError) -> Self {
        match e {
            toolkit_db::outbox::OutboxError::PayloadTooLarge { .. } => {
                Self::PayloadTooLarge(e.to_string())
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

/// Result alias of domain operations.
pub type DomainResult<T> = Result<T, DomainError>;
