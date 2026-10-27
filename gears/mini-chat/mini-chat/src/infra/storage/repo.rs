//! Repositories over the secure entities.

pub mod attachments;
pub mod chats;
pub mod messages;
pub mod quota;
pub mod reactions;
pub mod summaries;
pub mod turns;
pub mod vector_stores;

use toolkit_db::DBProvider;

use crate::error::DomainError;

/// The gear's database provider handle.
pub type Db = DBProvider<DomainError>;

/// Composite message ordering key `(created_at, id)`.
pub type MessageKey = (time::OffsetDateTime, uuid::Uuid);

/// Reduce a scope to its tenant part for child-table queries, preserving
/// an unconstrained scope (background/internal paths).
#[must_use]
pub fn tenant_scope(scope: &toolkit_db::secure::AccessScope) -> toolkit_db::secure::AccessScope {
    if scope.is_unconstrained() {
        toolkit_db::secure::AccessScope::allow_all()
    } else {
        scope.tenant_only()
    }
}

/// Helper: current UTC time.
#[must_use]
pub fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::now_utc()
}
