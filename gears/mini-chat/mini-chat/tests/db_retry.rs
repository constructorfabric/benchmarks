#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Lock contention on the per-test `SQLite` file (WAL, multi-connection
//! pool): a deferred transaction that read before another connection
//! committed fails its write with `SQLITE_BUSY_SNAPSHOT` (517);
//! `infra::db::tx::with_retry` re-runs it and it succeeds.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend};
use toolkit_db::contention::is_retryable_contention;
use uuid::Uuid;

use common::{chat_row, raw_strings, tenant_scope, test_db_with_raw, ts};
use mini_chat::domain::error::DomainError;
use mini_chat::infra::db::entity::chat;
use mini_chat::infra::db::repos::ChatRepo;
use mini_chat::infra::db::tx::with_retry;

/// Another connection commits a write to the chat row.
async fn concurrent_commit(raw: &DatabaseConnection, chat_id: Uuid) {
    let hex = chat_id.simple().to_string().to_uppercase();
    raw.execute_unprepared(&format!(
        "UPDATE chats SET title = 'concurrent' WHERE id = X'{hex}'"
    ))
    .await
    .unwrap();
}

async fn seeded() -> (common::TestDb, DatabaseConnection, chat::Model) {
    let (db, raw) = test_db_with_raw().await;
    let row = chat_row(Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(row.tenant_id, row.user_id);
    let row = ChatRepo
        .insert(&db.conn().unwrap(), &scope, row)
        .await
        .unwrap();
    (db, raw, row)
}

/// Read the chat, let another connection commit (first attempt only), then
/// write: the write of the first attempt hits `SQLITE_BUSY_SNAPSHOT`.
/// Boxed future of one transaction attempt.
type Attempt<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), DomainError>> + Send + 'a>>;

fn read_then_write(
    raw: DatabaseConnection,
    chat: chat::Model,
    attempts: Arc<AtomicU32>,
) -> impl for<'a> FnMut(&'a toolkit_db::DbTx<'a>) -> Attempt<'a> + Send {
    move |tx| {
        let (raw, chat, attempts) = (raw.clone(), chat.clone(), Arc::clone(&attempts));
        Box::pin(async move {
            let scope = tenant_scope(chat.tenant_id, chat.user_id);
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            ChatRepo.find_by_id(tx, &scope, chat.id).await?;
            if n == 1 {
                concurrent_commit(&raw, chat.id).await;
            }
            ChatRepo
                .rename(tx, &scope, chat.id, "mine", ts(1_700_000_500))
                .await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn plain_transaction_fails_with_busy_snapshot() {
    let (db, raw, chat) = seeded().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let mut body = read_then_write(raw.clone(), chat.clone(), Arc::clone(&attempts));

    let err = db.transaction(move |tx| body(tx)).await.unwrap_err();

    let db_err = err.db_err().expect("driver error preserved");
    assert!(
        is_retryable_contention(DbBackend::Sqlite, db_err),
        "{db_err}"
    );
    assert!(db_err.to_string().contains("517"), "{db_err}");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn with_retry_retries_busy_snapshot_and_succeeds() {
    let (db, raw, chat) = seeded().await;
    let attempts = Arc::new(AtomicU32::new(0));

    with_retry(
        &db,
        read_then_write(raw.clone(), chat.clone(), Arc::clone(&attempts)),
    )
    .await
    .expect("retried transaction commits");

    assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry");
    let hex = chat.id.simple().to_string().to_uppercase();
    assert_eq!(
        raw_strings(
            &raw,
            &format!("SELECT title FROM chats WHERE id = X'{hex}'")
        )
        .await,
        ["mine"]
    );
}

#[tokio::test]
async fn non_contention_errors_are_not_retried() {
    let (db, _raw, _chat) = seeded().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&attempts);

    let err = with_retry(&db, move |_tx| {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(DomainError::ChatNotFound)
        })
    })
    .await
    .unwrap_err();

    assert_eq!(err, DomainError::ChatNotFound);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// Black-box finding (Task 17): under concurrent load the outbox workers keep
/// the single `SQLite` writer busy, and a request transaction that already
/// read gets `SQLITE_BUSY` on its first write immediately (the busy timeout
/// does not apply to a read transaction upgrading to write). The real server
/// returned 500 `database is locked` once the old 8-attempt budget (~0.35 s
/// of jittered backoff) was spent. Another connection holds the write lock
/// for 1.5 s here; `with_retry` must outlast it and commit.
#[tokio::test]
async fn with_retry_outlasts_a_writer_holding_the_lock() {
    use sea_orm::TransactionTrait;

    let (db, raw, chat) = seeded().await;
    let hex = chat.id.simple().to_string().to_uppercase();

    let holder = raw.begin().await.unwrap();
    holder
        .execute_unprepared(&format!(
            "UPDATE chats SET title = 'holder' WHERE id = X'{hex}'"
        ))
        .await
        .unwrap();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel::<()>();
    let release = tokio::spawn(async move {
        locked_tx.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        holder.commit().await.unwrap();
    });
    locked_rx.await.unwrap();

    let attempts = Arc::new(AtomicU32::new(0));
    let counter = Arc::clone(&attempts);
    let started = std::time::Instant::now();
    with_retry(&db, move |tx| {
        let (chat, counter) = (chat.clone(), Arc::clone(&counter));
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            let scope = tenant_scope(chat.tenant_id, chat.user_id);
            ChatRepo.find_by_id(tx, &scope, chat.id).await?;
            ChatRepo
                .rename(tx, &scope, chat.id, "mine", ts(1_700_000_600))
                .await?;
            Ok(())
        })
    })
    .await
    .expect("transaction commits once the other writer released the lock");

    release.await.unwrap();
    assert!(
        attempts.load(Ordering::SeqCst) > 1,
        "contention was retried"
    );
    assert!(started.elapsed() >= std::time::Duration::from_millis(1000));
    assert_eq!(
        raw_strings(
            &raw,
            &format!("SELECT title FROM chats WHERE id = X'{hex}'")
        )
        .await,
        ["mine"]
    );
}
