//! The gear's write transactions.
//!
//! **Every write transaction of the gear MUST go through [`write_tx`] or [`write_tx_with_wakes`]**
//! (never `DBProvider::transaction` / `Db::transaction*` directly). Production runs `SQLite` in
//! WAL mode over a multi-connection pool, where a transaction that reads and then writes fails
//! at once with `SQLITE_BUSY` / `SQLITE_BUSY_SNAPSHOT` when another connection writes in between
//! (`busy_timeout` does not apply to that upgrade); `PostgreSQL` reports `40001` / `40P01`. These
//! failures are transient, so the helpers roll back and run the whole closure again in a new
//! transaction (`DEFAULT_TX_RETRY_ATTEMPTS` attempts, jittered backoff). Anything else
//! (unique violations, domain errors, ...) is returned at once.
//!
//! Consequences for the closure: it is called once per attempt and must start from scratch
//! (clone what it moves in; the first statement is typically the CAS or the reserve), it must
//! not have effects outside the transaction, and it must not hand out outbox wakes by any other
//! route than the [`TxWakes`] it is given: wakes of failed attempts are discarded, those of the
//! committed attempt are fired after the commit.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use sea_orm::DbErr;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::TxConfig;
use toolkit_db::{DBProvider, DbError, DbTx};

use crate::domain::error::DomainError;

/// The outbox wakes collected by one attempt of a [`write_tx_with_wakes`] closure.
pub struct TxWakes(Mutex<Wake>);

impl TxWakes {
    fn new() -> Self {
        Self(Mutex::new(Wake::empty()))
    }

    /// Adds the wake returned by `OutboxEnqueuer::enqueue`. It fires after the commit, or is
    /// discarded when the attempt fails.
    pub fn add(&self, wake: Wake) {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let current = std::mem::replace(&mut *slot, Wake::empty());
        *slot = current + wake;
    }

    fn take(&self) -> Wake {
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::replace(&mut *slot, Wake::empty())
    }
}

/// A failed attempt as the retry loop sees it: the domain error plus the database error text
/// handed to the platform classifier (`toolkit_db::contention::is_retryable_contention`).
/// Repositories flatten `DbErr` into `DomainError::Internal("database error: ...")`, so the
/// classifier is given that text as a `DbErr::Custom`, a shape it explicitly supports.
struct Attempted {
    error: DomainError,
    probe: Option<DbErr>,
}

/// `SQLITE_LOCKED` (shared-cache table lock, "database table is locked" / "database is
/// deadlocked") is as transient as `SQLITE_BUSY` but unknown to the platform classifier; it is
/// presented to it as the equivalent busy error.
const SQLITE_LOCKED_MARKERS: [&str; 3] = ["(code: 6)", "(code: 262)", "database is deadlocked"];
const SQLITE_BUSY_TEXT: &str = "(code: 5) database is locked";

fn probe_of(text: &str) -> DbErr {
    if SQLITE_LOCKED_MARKERS.iter().any(|m| text.contains(m)) {
        return DbErr::Custom(SQLITE_BUSY_TEXT.to_owned());
    }
    DbErr::Custom(text.to_owned())
}

impl From<DomainError> for Attempted {
    fn from(error: DomainError) -> Self {
        let probe = match &error {
            DomainError::Internal(text) => Some(probe_of(text)),
            _ => None,
        };
        Self { error, probe }
    }
}

impl From<DbError> for Attempted {
    fn from(err: DbError) -> Self {
        let probe = Some(probe_of(&err.to_string()));
        Self {
            error: err.into(),
            probe,
        }
    }
}

/// Runs `f` in a write transaction with contention retry (see the module docs).
///
/// # Errors
/// The closure's error, or a database error starting or committing the transaction.
pub async fn write_tx<T, F>(db: &DBProvider<DomainError>, mut f: F) -> Result<T, DomainError>
where
    T: Send + 'static,
    F: for<'a> FnMut(
            &'a DbTx<'a>,
        ) -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>
        + Send,
{
    write_tx_with_wakes(db, move |tx, _wakes| f(tx)).await
}

/// Like [`write_tx`], for closures that enqueue outbox messages: they add the wake of every
/// enqueue to the given [`TxWakes`]. After the commit the wakes of the committed attempt fire;
/// wakes of failed or retried attempts are discarded, so nothing is delivered for rolled-back
/// rows and no "Wake dropped" warning is logged.
///
/// # Errors
/// The closure's error, or a database error starting or committing the transaction.
pub async fn write_tx_with_wakes<T, F>(
    db: &DBProvider<DomainError>,
    mut f: F,
) -> Result<T, DomainError>
where
    T: Send + 'static,
    F: for<'a> FnMut(
            &'a DbTx<'a>,
            Arc<TxWakes>,
        ) -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>
        + Send,
{
    // The wakes of the latest attempt; replaced (and so discarded) by the next attempt.
    let latest: Arc<Mutex<Option<Arc<TxWakes>>>> = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&latest);
    let result = db
        .db()
        .transaction_with_retry(
            TxConfig::default(),
            |e: &Attempted| e.probe.as_ref(),
            move |tx| {
                let wakes = Arc::new(TxWakes::new());
                let previous = slot
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .replace(Arc::clone(&wakes));
                if let Some(previous) = previous {
                    previous.take().discard();
                }
                let attempt = f(tx, wakes);
                Box::pin(async move { attempt.await.map_err(Attempted::from) })
            },
        )
        .await;
    let wakes = latest.lock().unwrap_or_else(PoisonError::into_inner).take();
    match result {
        Ok(value) => {
            if let Some(wakes) = wakes {
                wakes.take().fire();
            }
            Ok(value)
        }
        Err(attempted) => {
            if let Some(wakes) = wakes {
                wakes.take().discard();
            }
            Err(attempted.error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use sea_orm::ActiveValue::Set;
    use sea_orm::EntityTrait;
    use toolkit_db::secure::{AccessScope, SecureEntityExt as _};
    use uuid::Uuid;

    use super::*;
    use crate::infra::db::entity::chats;
    use crate::infra::db::repo;
    use crate::infra::db::ts::db_now;
    use crate::infra::outbox::OutboxRecord;
    use crate::test_support::app::TestApp;
    use crate::test_support::db::test_db;
    use crate::test_support::fixtures::usage_event;

    /// What the driver reports for `SQLITE_BUSY`, in the shape the repositories wrap it in.
    const BUSY: &str = "database error: Execution Error: error returned from database: \
                        (code: 5) database is locked";
    const LOCKED: &str = "database error: Execution Error: error returned from database: \
                          (code: 6) database table is locked";

    fn chat_row() -> chats::ActiveModel {
        let now = db_now();
        chats::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(Uuid::new_v4()),
            user_id: Set(Uuid::new_v4()),
            model: Set("m".to_owned()),
            title: Set(None),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        }
    }

    async fn chat_count(db: &DBProvider<DomainError>) -> usize {
        let conn = db.conn().unwrap();
        chats::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
            .len()
    }

    /// Runs a transaction whose first `failures` attempts write a row and then fail with `err`;
    /// returns the result, the number of attempts and the rows left behind.
    async fn run(
        failures: u32,
        err: fn() -> DomainError,
    ) -> (Result<u32, DomainError>, u32, usize) {
        let test_db = test_db().await;
        let db = DBProvider::<DomainError>::new(test_db.db());
        let attempts = AtomicU32::new(0);
        let result = write_tx(&db, |tx| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            Box::pin(async move {
                repo::chats::insert(tx, &AccessScope::allow_all(), chat_row()).await?;
                if attempt <= failures {
                    return Err(err());
                }
                Ok(attempt)
            })
        })
        .await;
        let rows = chat_count(&db).await;
        (result, attempts.load(Ordering::SeqCst), rows)
    }

    #[tokio::test]
    async fn contention_is_retried_in_a_fresh_transaction() {
        let (result, attempts, rows) = run(2, || DomainError::Internal(BUSY.to_owned())).await;

        assert_eq!(result.ok(), Some(3), "third attempt commits");
        assert_eq!(attempts, 3);
        assert_eq!(
            rows, 1,
            "the writes of the failed attempts were rolled back"
        );
    }

    #[tokio::test]
    async fn sqlite_locked_is_retried_like_busy() {
        let (result, attempts, rows) = run(1, || DomainError::Internal(LOCKED.to_owned())).await;

        assert_eq!(result.ok(), Some(2));
        assert_eq!((attempts, rows), (2, 1));
    }

    #[tokio::test]
    async fn contention_that_outlasts_the_retry_budget_is_returned() {
        let (result, attempts, rows) =
            run(u32::MAX, || DomainError::Internal(BUSY.to_owned())).await;

        assert!(matches!(result, Err(DomainError::Internal(_))));
        assert_eq!(attempts, toolkit_db::DEFAULT_TX_RETRY_ATTEMPTS);
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn other_failures_are_not_retried() {
        let (result, attempts, rows) = run(u32::MAX, || {
            DomainError::Internal("database error: boom".to_owned())
        })
        .await;
        assert!(matches!(result, Err(DomainError::Internal(_))));
        assert_eq!((attempts, rows), (1, 0));

        let (result, attempts, _) = run(u32::MAX, || DomainError::Conflict {
            code: "unique_violation",
        })
        .await;
        assert!(matches!(result, Err(DomainError::Conflict { .. })));
        assert_eq!(attempts, 1, "a unique violation is final");

        let (result, attempts, _) = run(u32::MAX, || DomainError::TurnAlreadyRunning).await;
        assert!(matches!(result, Err(DomainError::TurnAlreadyRunning)));
        assert_eq!(attempts, 1);
    }

    /// The first attempt enqueues and then fails with contention: its outbox row is rolled back
    /// and its wake discarded, the retry enqueues again, and exactly one message is delivered.
    #[tokio::test]
    async fn contended_first_attempt_delivers_exactly_one_outbox_message() {
        let app = TestApp::builder().build().await;
        let attempts = AtomicU32::new(0);
        let tenant = Uuid::new_v4();
        let rec = OutboxRecord::usage(&usage_event(tenant)).unwrap();
        let outbox = Arc::clone(&app.services.outbox);

        let value = write_tx_with_wakes(&app.services.db, |tx, wakes| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            let (outbox, rec) = (Arc::clone(&outbox), rec.clone());
            Box::pin(async move {
                wakes.add(outbox.enqueue(tx, rec).await?);
                if attempt == 1 {
                    return Err(DomainError::Internal(BUSY.to_owned()));
                }
                Ok(attempt)
            })
        })
        .await
        .unwrap();
        assert_eq!(value, 2);

        let queue = app.services.cfg.outbox.queue_name.clone();
        TestApp::wait_until("the committed usage message is delivered", || async {
            !app.outbox_payloads(&queue).is_empty()
        })
        .await;
        // Give a (wrongly) surviving first-attempt row time to show up before counting.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(app.outbox_payloads(&queue).len(), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
