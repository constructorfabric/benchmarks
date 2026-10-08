//! Domain errors.
//!
//! Every variant maps to one row of the canonical error contract (ADR-0004);
//! the REST mapping lives in `api::rest::error`. Payload strings are internal
//! diagnostics (logged only), never sent to clients.

use std::sync::Arc;

use authz_resolver_sdk::EnforcerError;
use thiserror::Error;
use tracing::{debug, error};

/// Feature switched off by a kill switch (`failed_precondition` subject).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSubject {
    WebSearch,
    Images,
}

impl FeatureSubject {
    /// Wire subject (`web_search` / `images`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WebSearch => "web_search",
            Self::Images => "images",
        }
    }
}

/// Exhausted quota (`resource_exhausted` subject).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    Tokens,
    WebSearch,
    CodeInterpreter,
}

impl QuotaScope {
    /// Wire subject (`tokens` / `web_search` / `code_interpreter`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tokens => "tokens",
            Self::WebSearch => "web_search",
            Self::CodeInterpreter => "code_interpreter",
        }
    }
}

/// A client-side `toolkit_odata::Error` (never `Db` / `ParsingUnavailable`,
/// see `From<toolkit_odata::Error> for DomainError`). Equality compares the
/// rendered messages (the platform type is not `PartialEq`).
#[derive(Debug, Clone)]
pub struct ODataQueryError(pub toolkit_odata::Error);

impl PartialEq for ODataQueryError {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

impl Eq for ODataQueryError {}

impl std::fmt::Display for ODataQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A driver-level database error, kept intact (shared) so a transaction
/// retry can classify it (`toolkit_db::contention`). Equality compares the
/// rendered messages (`sea_orm::DbErr` is not `PartialEq`).
#[derive(Debug, Clone)]
pub struct SharedDbErr(pub Arc<sea_orm::DbErr>);

impl SharedDbErr {
    #[must_use]
    pub fn new(e: sea_orm::DbErr) -> Self {
        Self(Arc::new(e))
    }
}

impl PartialEq for SharedDbErr {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

impl Eq for SharedDbErr {}

impl std::fmt::Display for SharedDbErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Domain error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    // --- not_found (404) ---
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

    // --- invalid_argument (400) ---
    #[error("invalid model")]
    InvalidModel,
    #[error("empty content")]
    EmptyContent,
    #[error("invalid title")]
    InvalidTitle,
    #[error("invalid reaction")]
    InvalidReaction,
    #[error("invalid attachment")]
    InvalidAttachment,
    #[error("unsupported content type")]
    UnsupportedContentType,
    #[error("code interpreter unavailable")]
    CodeInterpreterUnavailable,
    #[error("multipart error: {field}/{reason}")]
    Multipart {
        field: &'static str,
        reason: &'static str,
    },
    #[error("vision not supported")]
    VisionNotSupported,
    #[error("outbox payload too large: {0}")]
    OutboxPayloadTooLarge(String),
    /// Invalid list query detected while paginating (unknown filter/order
    /// field, cursor mismatch, ...); rendered by the platform `OData` mapping.
    #[error("invalid list query: {0}")]
    InvalidQuery(ODataQueryError),

    // --- out_of_range (400) ---
    #[error("file too large")]
    FileTooLarge,
    #[error("too many images")]
    TooManyImages,
    #[error("input too long")]
    InputTooLong,
    #[error("context budget exceeded")]
    ContextBudgetExceeded,

    // --- failed_precondition (400) ---
    #[error("feature disabled: {}", .0.as_str())]
    FeatureDisabled(FeatureSubject),
    #[error("turn is not terminal")]
    TurnNotTerminal,
    #[error("reaction target is not an assistant message")]
    ReactionTargetNotAssistant,

    // --- permission_denied (403) / service_unavailable (503) ---
    #[error("authorization denied")]
    AuthzDenied,
    #[error("authorization service unavailable")]
    AuthzUnavailable,

    // --- aborted (409) ---
    #[error("another turn is running")]
    TurnAlreadyRunning,
    #[error("request id conflict")]
    RequestIdConflict,
    #[error("not the latest turn")]
    NotLatestTurn,
    #[error("generation in progress")]
    GenerationInProgress,
    #[error("replay of a completed turn")]
    Replay,

    // --- already_exists (409) ---
    #[error("attachment is locked")]
    AttachmentLocked,
    #[error("vector store provider mismatch")]
    ProviderMismatch,
    /// Unhandled unique-constraint violation; payload is the driver message.
    #[error("unique violation: {0}")]
    UniqueViolation(String),

    // --- resource_exhausted (429) ---
    #[error("quota exceeded: {}", .0.as_str())]
    QuotaExceeded(QuotaScope),
    #[error("document limit reached")]
    DocumentLimit,
    #[error("storage limit reached")]
    StorageLimit,

    // --- service_unavailable (503) ---
    #[error("storage backend unavailable")]
    StorageUnavailable,
    #[error("upload concurrency limit reached")]
    UploadConcurrencyLimit,

    // --- internal (500) ---
    /// A required plugin (model policy) could not be resolved or reached.
    #[error("plugin unavailable: {0}")]
    PluginUnavailable(String),
    /// Provider resolution failed before streaming.
    #[error("provider resolution failed: {0}")]
    ProviderResolution(String),
    /// Unexpected internal failure.
    #[error("internal error: {0}")]
    Internal(String),
    /// Database / persistence failure (infrastructure), message only.
    #[error("database error: {0}")]
    Database(String),
    /// Database failure with the driver error preserved (lock contention is
    /// retried by [`crate::infra::db::tx::with_retry`]).
    #[error("database error: {0}")]
    Db(SharedDbErr),
}

impl DomainError {
    /// Whether this is an (unhandled) unique-constraint violation.
    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::UniqueViolation(_))
    }

    /// The preserved driver error, if any (accessor for
    /// `Db::transaction_with_retry`).
    #[must_use]
    pub fn db_err(&self) -> Option<&sea_orm::DbErr> {
        match self {
            Self::Db(e) => Some(&e.0),
            _ => None,
        }
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(e: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&e) {
            Self::UniqueViolation(e.to_string())
        } else {
            Self::Db(SharedDbErr::new(e))
        }
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(e: toolkit_db::DbError) -> Self {
        match e {
            toolkit_db::DbError::Sea(db) => Self::from(db),
            e => Self::Database(e.to_string()),
        }
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(e: toolkit_db::secure::ScopeError) -> Self {
        match e {
            toolkit_db::secure::ScopeError::Db(db) => Self::from(db),
            e => Self::Database(e.to_string()),
        }
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(e: toolkit_odata::Error) -> Self {
        match e {
            toolkit_odata::Error::Db(msg) => Self::Database(msg),
            toolkit_odata::Error::ParsingUnavailable(msg) => Self::Internal(msg.to_owned()),
            e => Self::InvalidQuery(ODataQueryError(e)),
        }
    }
}

impl From<EnforcerError> for DomainError {
    fn from(e: EnforcerError) -> Self {
        match e {
            EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
                debug!(error = %e, "authorization denied");
                Self::AuthzDenied
            }
            EnforcerError::EvaluationFailed(_) => {
                error!(error = %e, "authorization evaluation failed");
                Self::AuthzUnavailable
            }
        }
    }
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
