#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(feature = "sqlite")]

//! Tests for [`Db::transaction_with_retry`] (and `_max`).
//!
//! These exercise the retry policy itself (extractor, attempt counting,
//! exhaustion, log on retry) using `sqlite::memory:`. The retryable case
//! constructs a real `SQLITE_BUSY` `DbErr` so that the helper's internal
//! call to [`toolkit_db::contention::is_retryable_contention`] flags it as
//! retryable for the `SQLite` backend.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use sea_orm::{DbErr, RuntimeErr};
use toolkit_db::{ConnectOpts, DEFAULT_TX_RETRY_ATTEMPTS, DbError, connect_db, secure::TxConfig};

#[derive(Debug)]
enum TestError {
    /// Wraps a real `DbErr` whose string representation is recognised by
    /// `is_retryable_contention` as a `SQLite` BUSY (code 5).
    Retryable(DbErr),
    Permanent,
    #[allow(dead_code)]
    Db(DbError),
}

impl From<DbError> for TestError {
    fn from(e: DbError) -> Self {
        TestError::Db(e)
    }
}

fn extract_db_err(e: &TestError) -> Option<&DbErr> {
    match e {
        TestError::Retryable(err) => Some(err),
        _ => None,
    }
}

fn sqlite_busy_err() -> DbErr {
    DbErr::Exec(RuntimeErr::Internal(
        "Execution Error: error returned from database: (code: 5) database is locked".to_owned(),
    ))
}

#[tokio::test]
async fn retry_default_succeeds_after_transient_failures() {
    // The default budget is `DEFAULT_TX_RETRY_ATTEMPTS` (= 3), so a body
    // that fails twice and succeeds on the third attempt must succeed without
    // the caller specifying a max.
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect sqlite memory");
    let counter = Arc::new(AtomicU32::new(0));

    let counter_for_body = Arc::clone(&counter);
    let result: Result<u32, TestError> = db
        .transaction_with_retry(TxConfig::default(), extract_db_err, move |_tx| {
            let counter = Arc::clone(&counter_for_body);
            Box::pin(async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n < DEFAULT_TX_RETRY_ATTEMPTS {
                    Err(TestError::Retryable(sqlite_busy_err()))
                } else {
                    Ok(n)
                }
            })
        })
        .await;

    assert!(
        matches!(result, Ok(n) if n == DEFAULT_TX_RETRY_ATTEMPTS),
        "got {result:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), DEFAULT_TX_RETRY_ATTEMPTS);
}

#[tokio::test]
async fn retry_returns_last_error_on_exhaustion() {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect sqlite memory");
    let counter = Arc::new(AtomicU32::new(0));

    let counter_for_body = Arc::clone(&counter);
    let result: Result<(), TestError> = db
        .transaction_with_retry_max(TxConfig::default(), 3, extract_db_err, move |_tx| {
            let counter = Arc::clone(&counter_for_body);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(TestError::Retryable(sqlite_busy_err()))
            })
        })
        .await;

    assert!(
        matches!(result, Err(TestError::Retryable(_))),
        "got {result:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn non_retryable_error_returns_immediately() {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect sqlite memory");
    let counter = Arc::new(AtomicU32::new(0));

    let counter_for_body = Arc::clone(&counter);
    let result: Result<(), TestError> = db
        .transaction_with_retry_max(TxConfig::default(), 3, extract_db_err, move |_tx| {
            let counter = Arc::clone(&counter_for_body);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(TestError::Permanent)
            })
        })
        .await;

    assert!(
        matches!(result, Err(TestError::Permanent)),
        "got {result:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn extractor_returning_none_skips_retry() {
    // A body whose error doesn't expose a `DbErr` (extractor → None) must
    // not be retried even if the helper has attempts remaining.
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect sqlite memory");
    let counter = Arc::new(AtomicU32::new(0));

    let counter_for_body = Arc::clone(&counter);
    let result: Result<(), TestError> = db
        .transaction_with_retry_max(TxConfig::default(), 3, extract_db_err, move |_tx| {
            let counter = Arc::clone(&counter_for_body);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(TestError::Db(DbError::InvalidConfig("boom".to_owned())))
            })
        })
        .await;

    assert!(matches!(result, Err(TestError::Db(_))), "got {result:?}");
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn zero_max_attempts_treated_as_one() {
    let db = connect_db("sqlite::memory:", ConnectOpts::default())
        .await
        .expect("connect sqlite memory");
    let counter = Arc::new(AtomicU32::new(0));

    let counter_for_body = Arc::clone(&counter);
    let result: Result<(), TestError> = db
        .transaction_with_retry_max(TxConfig::default(), 0, extract_db_err, move |_tx| {
            let counter = Arc::clone(&counter_for_body);
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(TestError::Retryable(sqlite_busy_err()))
            })
        })
        .await;

    assert!(
        matches!(result, Err(TestError::Retryable(_))),
        "got {result:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
