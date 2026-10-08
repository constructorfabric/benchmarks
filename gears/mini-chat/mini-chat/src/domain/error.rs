//! Domain error taxonomy of the mini-chat gear.
//!
//! The REST layer maps every variant onto the canonical error contract
//! (ADR-0004) in `api::rest::error`. Wire-visible texts never carry provider
//! identifiers; diagnostics stay in logs.

use thiserror::Error;
use uuid::Uuid;

/// Quota scope reported in `context.violations[].subject` of a 429.
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

/// Feature switched off by a kill switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisabledFeature {
    WebSearch,
    Images,
}

impl DisabledFeature {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::Images => "images",
        }
    }
}

/// Multipart parsing failure of an upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MultipartFailure {
    BoundaryRequired,
    Unreadable,
    MissingFile,
    MissingContentType,
}

#[derive(Debug, Error)]
pub enum DomainError {
    // --- not found -------------------------------------------------------
    #[error("chat not found")]
    ChatNotFound { id: Uuid },
    #[error("message not found")]
    MessageNotFound { id: Uuid },
    #[error("turn not found")]
    TurnNotFound { request_id: Uuid },
    #[error("attachment not found")]
    AttachmentNotFound { id: Uuid },
    #[error("model not found")]
    ModelNotFound { id: String },

    // --- invalid argument ------------------------------------------------
    #[error("invalid model: {detail}")]
    InvalidModel { detail: String },
    #[error("invalid chat title")]
    InvalidTitle,
    #[error("message content must not be empty")]
    EmptyContent,
    #[error("invalid reaction value")]
    InvalidReaction,
    #[error("invalid attachment: {detail}")]
    InvalidAttachment { detail: String },
    #[error("unsupported content type: {content_type}")]
    UnsupportedContentType { content_type: String },
    #[error("code interpreter unavailable for this upload")]
    CodeInterpreterUnavailable,
    #[error("invalid multipart request: {detail}")]
    Multipart {
        failure: MultipartFailure,
        detail: String,
    },
    #[error("outbox payload too large: {detail}")]
    OutboxPayloadTooLarge { detail: String },
    #[error("the model does not support image input")]
    VisionNotSupported,

    // --- out of range ----------------------------------------------------
    #[error("file too large (limit {limit_bytes} bytes)")]
    FileTooLarge { limit_bytes: u64 },
    #[error("too many images in one message (max {max})")]
    TooManyImages { max: u32 },
    #[error("message exceeds the model input limit ({limit} tokens)")]
    InputTooLong { limit: u32 },
    #[error("mandatory context does not fit the token budget")]
    ContextBudgetExceeded,

    // --- failed precondition -------------------------------------------
    #[error("feature disabled: {}", .feature.as_str())]
    FeatureDisabled { feature: DisabledFeature },
    #[error("the turn is not in a terminal state")]
    TurnNotTerminal,
    #[error("reactions are allowed on assistant messages only")]
    ReactionTargetNotAssistant,

    // --- authorization ---------------------------------------------------
    #[error("access denied")]
    AccessDenied,
    #[error("authorization service unavailable: {detail}")]
    AuthzUnavailable { detail: String },

    // --- aborted ---------------------------------------------------------
    #[error("another turn is running in this chat")]
    TurnAlreadyRunning,
    #[error("request_id conflict: {detail}")]
    RequestIdConflict { detail: String },
    #[error("the turn is not the latest turn of the chat")]
    NotLatestTurn,
    #[error("a concurrent generation is in progress")]
    GenerationInProgress,
    #[error("completed turn is replayed")]
    Replay,

    // --- already exists --------------------------------------------------
    #[error("attachment is referenced by a submitted message")]
    AttachmentLocked,
    #[error("the chat vector store belongs to another provider backend")]
    ProviderMismatch,
    #[error("unique constraint violation: {detail}")]
    UniqueViolation { detail: String },

    // --- resource exhausted ---------------------------------------------
    #[error("quota exceeded ({})", .scope.as_str())]
    QuotaExceeded { scope: QuotaScope },
    #[error("per-chat document limit reached")]
    DocumentLimit,
    #[error("per-chat storage limit reached")]
    StorageLimit,

    // --- unavailable -----------------------------------------------------
    #[error("storage backend unavailable: {detail}")]
    StorageUnavailable { detail: String },
    #[error("too many concurrent uploads")]
    UploadConcurrencyLimit,

    // --- internal --------------------------------------------------------
    #[error("provider resolution failed: {detail}")]
    ProviderResolution { detail: String },
    #[error("policy resolution failed: {detail}")]
    PolicyResolution { detail: String },
    #[error("internal error: {detail}")]
    Internal { detail: String },
    /// The assistant message could not be persisted at finalization.
    #[error("message persistence failed: {detail}")]
    MessagePersistence { detail: String },
    /// A database error (kept for contention classification and retry).
    #[error("database error: {0}")]
    Database(sea_orm::DbErr),
    /// An `OData` / pagination error (already canonical).
    #[error("odata: {0}")]
    OData(toolkit_odata::Error),
}

impl DomainError {
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::Internal {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn invalid_attachment(detail: impl Into<String>) -> Self {
        Self::InvalidAttachment {
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn invalid_model(detail: impl Into<String>) -> Self {
        Self::InvalidModel {
            detail: detail.into(),
        }
    }

    /// The underlying database error, if any (transaction retry
    /// classification).
    #[must_use]
    pub fn db_err(&self) -> Option<&sea_orm::DbErr> {
        match self {
            Self::Database(e) => Some(e),
            _ => None,
        }
    }

    /// `true` for errors that must not be surfaced as a stored turn error
    /// code but as a JSON error only.
    #[must_use]
    pub fn is_quota_exceeded(&self) -> bool {
        matches!(self, Self::QuotaExceeded { .. })
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        if err.is_unique_violation() {
            return Self::UniqueViolation {
                detail: err.to_string(),
            };
        }
        match err {
            toolkit_db::secure::ScopeError::Db(db) => Self::Database(db),
            toolkit_db::secure::ScopeError::Denied(_)
            | toolkit_db::secure::ScopeError::TenantNotInScope { .. } => Self::AccessDenied,
            other => Self::Internal {
                detail: format!("database error: {other}"),
            },
        }
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        let msg = err.to_string();
        if let toolkit_db::DbError::Sea(db) = err {
            if toolkit_db::secure::is_unique_violation(&db) {
                return Self::UniqueViolation { detail: msg };
            }
            return Self::Database(db);
        }
        Self::Internal {
            detail: format!("database error: {msg}"),
        }
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::OData(err)
    }
}

impl From<authz_resolver_sdk::EnforcerError> for DomainError {
    fn from(err: authz_resolver_sdk::EnforcerError) -> Self {
        match err {
            authz_resolver_sdk::EnforcerError::Denied { .. }
            | authz_resolver_sdk::EnforcerError::CompileFailed(_) => {
                tracing::warn!(error = %err, "mini-chat: authorization denied");
                Self::AccessDenied
            }
            authz_resolver_sdk::EnforcerError::EvaluationFailed(e) => {
                tracing::error!(error = %e, "mini-chat: authorization evaluation failed");
                Self::AuthzUnavailable {
                    detail: e.to_string(),
                }
            }
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;
