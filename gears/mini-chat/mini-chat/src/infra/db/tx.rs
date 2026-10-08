//! Whole-transaction retry on database contention.
//!
//! `SQLite` (WAL) runs a deferred transaction on a read snapshot. When another
//! connection commits between the transaction's first read and its first
//! write, the upgrade to a write transaction fails at once with
//! `SQLITE_BUSY_SNAPSHOT` (code 517; `busy_timeout` does not apply), and an
//! upgrade while another connection holds the write lock fails with
//! `SQLITE_BUSY` (code 5). `PostgreSQL` reports serialization failures and
//! deadlocks the same way. The database has rolled the transaction back; the
//! only remedy is to run it again from `BEGIN` on a fresh snapshot. toolkit-db
//! offers no `BEGIN IMMEDIATE`, so every transaction of the gear runs through
//! [`with_tx_retry`].
//!
//! Each attempt invokes the closure again with a fresh transaction, so the
//! closure must be idempotent: values it moves into the transaction are cloned
//! per attempt, ids are allocated outside it, and outbox wakes are returned
//! as part of the result ([`PendingWakes`](crate::infra::outbox::PendingWakes)),
//! so only the committed attempt's wakes reach the caller; a failed attempt's
//! wakes are discarded unfired when its result is dropped.

use std::fmt::Debug;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use toolkit_db::secure::DbTx;
use toolkit_db::{DBProvider, DbError};
use tracing::warn;

use crate::domain::error::DomainError;

/// Attempts of one transaction (the first try included).
pub const MAX_TX_ATTEMPTS: u32 = 5;

/// Backoff before the second attempt; doubles per further attempt up to
/// [`BACKOFF_CAP`]. Half of each delay is fixed and half random, so two
/// transactions that just collided restart apart, and the writer that won
/// (often an outbox worker committing a batch) gets time to finish.
const BACKOFF_BASE: Duration = Duration::from_millis(20);
const BACKOFF_CAP: Duration = Duration::from_millis(200);

/// One transaction attempt: the future returned by the closure for `tx`.
pub type TxFuture<'a, T, E> = Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>;

/// Errors of a transaction closure that tell retryable contention apart.
pub trait TxRetryError {
    /// `true` only for transient database contention; never for an error the
    /// closure produced itself.
    fn is_contention(&self) -> bool;
}

impl TxRetryError for DomainError {
    fn is_contention(&self) -> bool {
        matches!(self, Self::DbContention(_))
    }
}

/// Run `attempt` in a transaction; when it fails with database contention
/// (begin, body or commit), run it again in a new transaction, up to
/// [`MAX_TX_ATTEMPTS`] attempts with a short jittered backoff. Any other error
/// returns at once. `label` names the transaction in the logs.
///
/// # Errors
/// The error of the last attempt.
pub async fn with_tx_retry<T, E, F>(
    db: &DBProvider<DomainError>,
    label: &'static str,
    mut attempt: F,
) -> Result<T, E>
where
    T: Send + 'static,
    E: From<DbError> + TxRetryError + Debug + Send + 'static,
    F: for<'a> FnMut(&'a DbTx<'a>) -> TxFuture<'a, T, E> + Send,
{
    let db = db.db();
    let mut n: u32 = 1;
    loop {
        let err = match db.transaction_ref_mapped(|tx| attempt(tx)).await {
            Ok(value) => return Ok(value),
            Err(err) => err,
        };
        if !err.is_contention() {
            return Err(err);
        }
        if n >= MAX_TX_ATTEMPTS {
            warn!(tx = label, attempts = n, error = ?err, "transaction still contended; giving up");
            return Err(err);
        }
        let delay = backoff(n);
        warn!(tx = label, attempt = n, ?delay, error = ?err, "transaction hit database contention; retrying it");
        tokio::time::sleep(delay).await;
        n += 1;
    }
}

/// Delay after failed attempt `n` (1-based): half of `BASE * 2^(n-1)` (capped)
/// plus a random part of the other half.
fn backoff(n: u32) -> Duration {
    let nominal = BACKOFF_BASE
        .saturating_mul(1 << n.saturating_sub(1).min(8))
        .min(BACKOFF_CAP);
    let half = nominal / 2;
    half + tokio_retry::strategy::jitter(half)
}

#[cfg(test)]
#[path = "tx_tests.rs"]
mod tx_tests;
