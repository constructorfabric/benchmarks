//! Persistence: entities, migrations, repositories.

pub mod entities;
pub mod migrations;
pub mod odata;

use time::OffsetDateTime;

/// Current UTC time with a fixed-width fractional part.
///
/// SQLite stores timestamps as RFC 3339 text and the driver trims trailing
/// fractional zeros, which breaks lexicographic ordering. Forcing the last
/// nanosecond digit to be non-zero keeps exactly nine fractional digits, so
/// text order equals chronological order.
#[must_use]
pub fn now() -> OffsetDateTime {
    normalize_ts(OffsetDateTime::now_utc())
}

/// Normalizes a timestamp to UTC with nine fractional digits (see [`now`]).
#[must_use]
pub fn normalize_ts(ts: OffsetDateTime) -> OffsetDateTime {
    let ts = ts.to_offset(time::UtcOffset::UTC);
    let ns = ts.nanosecond();
    let ns = ns - ns % 10 + 1;
    ts.replace_nanosecond(ns).unwrap_or(ts)
}

use std::future::Future;
use std::pin::Pin;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::DBProvider;
use toolkit_db::secure::{DbTx, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::domain::error::DomainError;

/// Write transactions that take the write lock with their first statement.
///
/// A SQLite deferred transaction that reads first and writes later fails with
/// `SQLITE_BUSY_SNAPSHOT` (not retried by `busy_timeout`) when another connection —
/// for example an outbox worker — committed in between. Starting every write
/// transaction with a no-op `UPDATE` acquires the write lock up front (waiting via
/// `busy_timeout`), which is the `BEGIN IMMEDIATE` behavior. On Postgres the
/// statement matches no rows and takes no locks.
#[async_trait::async_trait]
pub trait WriteTransaction {
    /// Runs `f` in a write transaction.
    async fn write_transaction<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>> + Send + 'static;
}

/// Acquires the write lock of the current transaction.
///
/// # Errors
/// Database errors.
pub async fn begin_write(tx: &DbTx<'_>) -> Result<(), DomainError> {
    entities::chats::Entity::update_many()
        .secure()
        .col_expr(entities::chats::Column::Title, Expr::col(entities::chats::Column::Title))
        .filter(Condition::all().add(entities::chats::Column::Id.is_null()))
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await?;
    Ok(())
}

#[async_trait::async_trait]
impl WriteTransaction for DBProvider<DomainError> {
    async fn write_transaction<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>> + Send + 'static,
    {
        self.transaction(move |tx| {
            Box::pin(async move {
                begin_write(tx).await?;
                f(tx).await
            })
        })
        .await
    }
}
