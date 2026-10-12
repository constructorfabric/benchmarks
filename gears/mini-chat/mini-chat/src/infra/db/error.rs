//! Classification of driver errors into domain errors.

use sea_orm::{DbErr, SqlErr};

use crate::domain::error::DomainError;

/// Conflict code reported for unique-constraint violations.
pub use crate::domain::error::UNIQUE_VIOLATION;

/// Maps a `DbErr` to a [`DomainError`].
///
/// Unique violations (PK and unique index) become `Conflict { unique_violation }`;
/// everything else is an opaque `Internal` error whose cause is only logged.
#[must_use]
pub fn classify_db_err(err: DbErr) -> DomainError {
    if matches!(err.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) {
        tracing::warn!(error = %err, "unique constraint violation");
        return DomainError::Conflict {
            code: UNIQUE_VIOLATION,
        };
    }
    tracing::warn!(error = %err, "database error");
    DomainError::Internal {
        diagnostic: "database error".to_owned(),
        cause: Some(Box::new(err)),
    }
}

impl From<toolkit_db::DbError> for DomainError {
    fn from(err: toolkit_db::DbError) -> Self {
        match err {
            toolkit_db::DbError::Sea(db) => classify_db_err(db),
            other => {
                tracing::warn!(error = %other, "database error");
                Self::Internal {
                    diagnostic: "database error".to_owned(),
                    cause: Some(Box::new(other)),
                }
            }
        }
    }
}

impl From<toolkit_db::secure::ScopeError> for DomainError {
    fn from(err: toolkit_db::secure::ScopeError) -> Self {
        match err {
            toolkit_db::secure::ScopeError::Db(db) => classify_db_err(db),
            other => {
                tracing::warn!(error = %other, "secure scope error");
                Self::Internal {
                    diagnostic: "database scope error".to_owned(),
                    cause: Some(Box::new(other)),
                }
            }
        }
    }
}

/// `PayloadTooLarge` is the only outbox failure a caller can act on (the
/// chat-delete endpoint reports it as a 400); every other failure (queue not
/// registered, driver error, ...) is an opaque internal error.
impl From<toolkit_db::outbox::OutboxError> for DomainError {
    fn from(err: toolkit_db::outbox::OutboxError) -> Self {
        match err {
            toolkit_db::outbox::OutboxError::PayloadTooLarge { size, max } => {
                Self::OutboxPayloadTooLarge {
                    message: format!("payload size {size} exceeds maximum {max}"),
                }
            }
            // The driver error stays the cause so a transaction can recognize
            // retryable contention (see `DomainError::db_cause`).
            toolkit_db::outbox::OutboxError::Database(db) => {
                tracing::warn!(error = %db, "outbox database error");
                Self::Internal {
                    diagnostic: "outbox error".to_owned(),
                    cause: Some(Box::new(db)),
                }
            }
            other => {
                tracing::warn!(error = %other, "outbox error");
                Self::Internal {
                    diagnostic: "outbox error".to_owned(),
                    cause: Some(Box::new(other)),
                }
            }
        }
    }
}

impl DomainError {
    /// The driver error behind an internal error, if any. Lets the transaction
    /// helper of `toolkit-db` recognize retryable lock contention.
    #[must_use]
    pub fn db_cause(&self) -> Option<&DbErr> {
        match self {
            Self::Internal {
                cause: Some(cause), ..
            } => cause.downcast_ref::<DbErr>(),
            _ => None,
        }
    }
}
