//! Persistence layer: entities, migrations and repositories.

use sea_orm::DbErr;
use time::OffsetDateTime;
use toolkit_db::secure::{ScopeError, is_unique_violation};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, Res};

pub mod entities;
pub mod migrations;
pub mod odata;
pub mod repo;

/// Database provider with mini-chat domain errors.
pub type Db = toolkit_db::DBProvider<DomainError>;

/// Current UTC time for persisted timestamps.
///
/// Truncated to microseconds (the PostgreSQL precision) plus one nanosecond:
/// SQLite stores the RFC 3339 text that `time` produces, which drops trailing
/// fractional zeros; a fixed nine-digit fraction keeps the text sortable in
/// the same order as the instants.
#[must_use]
pub fn now_ts() -> OffsetDateTime {
    let now = OffsetDateTime::now_utc();
    #[allow(clippy::integer_division)] // truncation to microseconds is intended
    let micros = now.nanosecond() / 1_000;
    now.replace_nanosecond(micros * 1_000 + 1).unwrap_or(now)
}

/// Tenant-only access scope for chat child tables (owner isolation comes from
/// the owner-scoped chat lookup).
#[must_use]
pub fn tenant_scope(tenant_id: Uuid) -> AccessScope {
    AccessScope::for_tenant(tenant_id)
}

/// System scope for background workers that operate across tenants.
#[must_use]
pub fn system_scope() -> AccessScope {
    AccessScope::allow_all()
}

#[allow(clippy::needless_pass_by_value)] // used as a `map_err` function
pub fn classify_db_err(err: DbErr) -> DomainError {
    if toolkit_db::contention::is_retryable_contention(sea_orm::DbBackend::Sqlite, &err)
        || toolkit_db::contention::is_retryable_contention(sea_orm::DbBackend::Postgres, &err)
    {
        return DomainError::Contention(err.to_string());
    }
    if is_unique_violation(&err) {
        tracing::debug!(error = %err, "unique violation");
        return DomainError::AlreadyExists {
            res: Res::Chat,
            name: "unique_violation".to_owned(),
            detail: "The resource conflicts with an existing one".to_owned(),
        };
    }
    DomainError::Internal(format!("database error: {err}"))
}

pub fn map_scope_err(err: ScopeError) -> DomainError {
    match err {
        ScopeError::Db(db) => classify_db_err(db),
        other => DomainError::Internal(format!("scope error: {other}")),
    }
}

/// Run a transactional operation, retrying on write contention.
pub async fn with_retry<T, F, Fut>(mut op: F) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, DomainError>>,
{
    let mut attempt = 0u32;
    loop {
        match op().await {
            Err(DomainError::Contention(e)) if attempt < 6 => {
                attempt += 1;
                tracing::debug!(attempt, error = %e, "retrying after write contention");
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
