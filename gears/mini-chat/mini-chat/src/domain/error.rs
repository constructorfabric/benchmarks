//! Domain error taxonomy. Mapped to canonical `Problem`s in
//! `api::rest::error` (ADR-0004).

use toolkit_canonical_errors::CanonicalError;
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;

/// Resource kinds that appear in `context.resource_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Res {
    Chat,
    Message,
    Turn,
    Attachment,
    Model,
}

#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    #[error("{0:?} not found")]
    NotFound(Res),

    #[error("invalid argument {field}: {reason}")]
    InvalidArgument {
        res: Res,
        field: String,
        reason: String,
        description: String,
    },

    #[error("out of range {field}: {reason}")]
    OutOfRange {
        res: Res,
        field: String,
        reason: String,
        description: String,
    },

    #[error("failed precondition {subject}/{kind}")]
    FailedPrecondition {
        res: Res,
        subject: String,
        kind: String,
        description: String,
    },

    #[error("permission denied")]
    PermissionDenied,

    #[error("authorization service unavailable")]
    PdpUnavailable,

    #[error("aborted: {reason}")]
    Aborted { res: Res, reason: String, detail: String },

    #[error("already exists: {resource_name}")]
    AlreadyExists { res: Res, resource_name: String, detail: String },

    #[error("quota exceeded: {scope}")]
    QuotaExceeded { scope: String },

    #[error("resource exhausted: {subject}")]
    LimitExceeded { res: Res, subject: String, description: String },

    #[error("service unavailable: {detail}")]
    ServiceUnavailable { retry_after: u64, detail: String },

    #[error("internal error: {0}")]
    Internal(String),

    /// Database failure other than a unique violation (kept for contention retries).
    #[error("database error: {0}")]
    Database(sea_orm::DbErr),

    /// Pre-built canonical error (`OData` errors, extractor errors).
    #[error("{0}")]
    Canonical(CanonicalError),
}

impl DomainError {
    #[must_use]
    pub fn invalid(res: Res, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::InvalidArgument {
            res,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn out_of_range(res: Res, field: &str, reason: &str, description: impl Into<String>) -> Self {
        Self::OutOfRange {
            res,
            field: field.to_owned(),
            reason: reason.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn precondition(res: Res, subject: &str, kind: &str, description: impl Into<String>) -> Self {
        Self::FailedPrecondition {
            res,
            subject: subject.to_owned(),
            kind: kind.to_owned(),
            description: description.into(),
        }
    }

    #[must_use]
    pub fn aborted(res: Res, reason: &str, detail: impl Into<String>) -> Self {
        Self::Aborted { res, reason: reason.to_owned(), detail: detail.into() }
    }

    #[must_use]
    pub fn invalid_model() -> Self {
        Self::invalid(Res::Chat, "model", "INVALID_MODEL", "model is not available in the catalog")
    }

    #[must_use]
    pub fn feature_disabled(subject: &str) -> Self {
        Self::precondition(Res::Chat, subject, "FEATURE_DISABLED", format!("{subject} is disabled"))
    }

    #[must_use]
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// The underlying database error, if any (used by transaction retries).
    #[must_use]
    pub fn db_err(&self) -> Option<&sea_orm::DbErr> {
        match self {
            Self::Database(e) => Some(e),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_unique_violation(&self) -> bool {
        matches!(self, Self::AlreadyExists { resource_name, .. } if resource_name == "unique_violation")
    }
}

impl From<sea_orm::DbErr> for DomainError {
    fn from(err: sea_orm::DbErr) -> Self {
        if toolkit_db::secure::is_unique_violation(&err) {
            tracing::debug!(error = %err, "unique violation");
            return Self::AlreadyExists {
                res: Res::Chat,
                resource_name: "unique_violation".to_owned(),
                detail: "The resource already exists".to_owned(),
            };
        }
        Self::Database(err)
    }
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(e) => e.into(),
            other => Self::Internal(format!("database error: {other}")),
        }
    }
}

impl From<ScopeError> for DomainError {
    fn from(err: ScopeError) -> Self {
        match err {
            ScopeError::Db(e) => e.into(),
            other => Self::Internal(format!("scope error: {other}")),
        }
    }
}

impl From<CanonicalError> for DomainError {
    fn from(err: CanonicalError) -> Self {
        Self::Canonical(err)
    }
}

impl From<toolkit_odata::Error> for DomainError {
    fn from(err: toolkit_odata::Error) -> Self {
        Self::Canonical(CanonicalError::from(err))
    }
}

pub type DomainResult<T> = Result<T, DomainError>;
