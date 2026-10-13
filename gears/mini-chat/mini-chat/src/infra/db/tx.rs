//! Gear transactions with bounded retry on lock contention.
//!
//! On `SQLite` (WAL) a deferred transaction that read before another
//! connection committed fails its first write with `SQLITE_BUSY_SNAPSHOT`
//! immediately (the busy timeout does not apply); the outbox workers commit
//! concurrently with request transactions. [`with_retry`] runs the body with
//! toolkit-db's `Db::transaction_with_retry_max` (`TxConfig::default()`),
//! which re-runs the whole transaction on a retryable contention error
//! (`SQLite` `BUSY` / `BUSY_SNAPSHOT`, `PostgreSQL` serialization failure /
//! deadlock). The driver error must reach it intact: repository errors
//! convert to [`DomainError::Db`].
//!
//! The budget is [`TX_RETRY_ATTEMPTS`] instead of the workspace default 3.
//! The outbox workers (5 queues x 4 partitions) keep the single `SQLite`
//! writer busy with short write transactions: in one burst when the
//! pipeline starts, and continuously while several turns finalize at once.
//! A transaction that already read gets `SQLITE_BUSY` on its first write
//! immediately (the busy timeout does not apply to a read transaction that
//! upgrades to write), so only the retry loop waits for the writer. Each
//! retry waits a jittered delay of at most 100 ms (about 50 ms on average
//! once the backoff is capped), so the budget lets a transaction outlast
//! roughly 3 s of continuous write contention, the same order as the
//! `SQLite` `busy_timeout` of the shipped configs. The former budget of 8
//! (about 0.35 s) was exhausted under concurrent load on the real server,
//! which answered 500 `database is locked`.
//!
//! The body runs once per attempt in a fresh transaction, so it must not
//! carry state between attempts: clone captured inputs per call, build
//! `PendingWakes` inside it, and fire them only after `with_retry` returned
//! `Ok` (the commit succeeded).

use std::future::Future;
use std::pin::Pin;

use toolkit_db::secure::TxConfig;
use toolkit_db::{DBProvider, DbTx};

use crate::domain::error::DomainError;

/// Attempt budget (first try included) — see the module docs.
pub const TX_RETRY_ATTEMPTS: u32 = 64;

/// Run `body` in a transaction, retrying the whole transaction on lock
/// contention (see the module docs).
///
/// # Errors
///
/// The body's error, a non-retryable database error, or the last
/// contention error once the attempt budget is spent.
pub async fn with_retry<T, F>(db: &DBProvider<DomainError>, body: F) -> Result<T, DomainError>
where
    T: Send + 'static,
    F: for<'a> FnMut(
            &'a DbTx<'a>,
        ) -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>
        + Send,
{
    db.db()
        .transaction_with_retry_max(
            TxConfig::default(),
            TX_RETRY_ATTEMPTS,
            DomainError::db_err,
            body,
        )
        .await
}
