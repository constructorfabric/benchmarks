//! Persistence: entities, migrations and shared helpers.

pub mod entity;
pub mod migrations;

use time::OffsetDateTime;
use toolkit_db::DBProvider;

use crate::domain::error::DomainError;

/// Gear database handle.
pub type MiniChatDb = DBProvider<DomainError>;

/// Current time truncated to microseconds (stable across PG and SQLite).
///
/// Timestamps are truncated to microseconds and never end in a zero digit so
/// `SQLite`'s RFC 3339 text always has six fraction digits and sorts
/// lexicographically in time order.
#[must_use]
pub fn now() -> OffsetDateTime {
    let t = OffsetDateTime::now_utc();
    let mut micros = t.microsecond();
    if micros.is_multiple_of(10) {
        micros += 1;
    }
    t.replace_nanosecond(micros * 1000).unwrap_or(t)
}

/// Attempt budget for gear write transactions. `SQLite` reports a WAL snapshot
/// upgrade conflict (`SQLITE_BUSY_SNAPSHOT`) immediately instead of waiting
/// on `busy_timeout`, and the outbox workers write concurrently, so a
/// read-then-write transaction is retried from `BEGIN`.
const TX_ATTEMPTS: u32 = 8;

/// Run `body` in a transaction, retrying the whole transaction on lock
/// contention. `body` runs once per attempt, so it must clone what it moves.
///
/// # Errors
/// The body's error, or the last contention error after the attempt budget.
pub async fn tx_retry<T, F>(db: &MiniChatDb, body: F) -> Result<T, DomainError>
where
    T: Send + 'static,
    F: for<'a> FnMut(
            &'a toolkit_db::secure::DbTx<'a>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, DomainError>> + Send + 'a>>
        + Send,
{
    db.db()
        .transaction_with_retry_max(toolkit_db::secure::TxConfig::default(), TX_ATTEMPTS, DomainError::db_err, body)
        .await
}
