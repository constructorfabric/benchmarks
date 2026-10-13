//! Classification of database errors into domain errors.

use sea_orm::DbErr;
use toolkit_db::DbError;
use toolkit_db::secure::{ScopeError, is_unique_violation};
use tracing::{error, warn};

use crate::domain::error::{DomainError, ProtectedResource};

/// Classifies a raw database error. The driver text is only logged; it never reaches a client.
pub fn classify_db_err(err: &DbErr) -> DomainError {
    if is_unique_violation(err) {
        warn!(error = %err, "unique constraint violation");
        return DomainError::UniqueViolation;
    }
    error!(error = %err, "database error");
    DomainError::internal("database error")
}

/// Maps a Secure ORM error. A scope violation means the compiled authorization scope refused the
/// operation; everything else is an internal error.
#[allow(
    clippy::needless_pass_by_value,
    reason = "used directly as a `map_err` callback"
)]
pub fn map_scope_err(err: ScopeError) -> DomainError {
    match err {
        ScopeError::Db(db) => classify_db_err(&db),
        ScopeError::TenantNotInScope { .. } | ScopeError::Denied(_) => {
            warn!(error = %err, "secure ORM refused the operation");
            DomainError::AuthzDenied {
                resource: ProtectedResource::Chat,
            }
        }
        other => {
            error!(error = %other, "invalid secure ORM scope");
            DomainError::internal("invalid access scope")
        }
    }
}

impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(db) => classify_db_err(&db),
            other => {
                error!(error = %other, "database error");
                Self::internal("database error")
            }
        }
    }
}
