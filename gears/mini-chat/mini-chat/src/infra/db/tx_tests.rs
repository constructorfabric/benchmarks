#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use toolkit_db::secure::AccessScope;
use toolkit_db::{DBProvider, Db};
use uuid::Uuid;

use super::{MAX_TX_ATTEMPTS, TxRetryError, with_tx_retry};
use crate::domain::error::DomainError;
use crate::infra::db::repos::chat::{ChatRepo, NewChat};
use crate::infra::db::{file_test_db, test_db};

const TENANT: Uuid = Uuid::from_u128(1);
const USER: Uuid = Uuid::from_u128(2);

fn t(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap() + chrono::Duration::seconds(secs)
}

async fn chat(db: &Db) -> Uuid {
    let id = Uuid::new_v4();
    ChatRepo::insert(
        &db.conn().unwrap(),
        &AccessScope::allow_all(),
        NewChat {
            id,
            tenant_id: TENANT,
            user_id: USER,
            model: "m".to_owned(),
            title: None,
            now: t(0),
        },
    )
    .await
    .unwrap();
    id
}

/// Commit a write on another pooled connection (a separate task: the
/// transaction guard forbids `conn()` inside the transaction's task).
async fn concurrent_write(db: &Db, chat_id: Uuid, at: DateTime<Utc>) {
    let db = db.clone();
    tokio::spawn(async move {
        ChatRepo::touch(&db.conn().unwrap(), TENANT, chat_id, at)
            .await
            .unwrap();
    })
    .await
    .unwrap();
}

async fn updated_at(db: &Db, chat_id: Uuid) -> DateTime<Utc> {
    ChatRepo::find_any(&db.conn().unwrap(), TENANT, chat_id)
        .await
        .unwrap()
        .unwrap()
        .updated_at
}

/// Read (opens the WAL read snapshot), let another connection commit when
/// `interfere`, then write: the snapshot can no longer be upgraded and
/// `SQLite` fails the write with `SQLITE_BUSY_SNAPSHOT`.
async fn read_then_write(
    provider: &DBProvider<DomainError>,
    db: &Db,
    attempts: &Arc<AtomicU32>,
    interfere: impl Fn(u32) -> bool + Send + Sync + Copy + 'static,
    (mine, other): (Uuid, Uuid),
) -> Result<u32, DomainError> {
    with_tx_retry(provider, "test read-then-write", |tx| {
        let attempts = Arc::clone(attempts);
        let db = db.clone();
        Box::pin(async move {
            let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            ChatRepo::find_any(tx, TENANT, mine)
                .await?
                .ok_or(DomainError::ChatNotFound)?;
            if interfere(n) {
                concurrent_write(&db, other, t(i64::from(n))).await;
            }
            ChatRepo::touch(tx, TENANT, mine, t(100)).await?;
            Ok(n)
        })
    })
    .await
}

#[tokio::test]
async fn retries_on_sqlite_busy_then_succeeds() {
    let (db, _dir) = file_test_db(4).await;
    let (mine, other) = (chat(&db).await, chat(&db).await);
    let provider = DBProvider::<DomainError>::new(db.clone());
    let attempts = Arc::new(AtomicU32::new(0));

    let committed = read_then_write(&provider, &db, &attempts, |n| n == 1, (mine, other))
        .await
        .expect("the second attempt commits");

    assert_eq!(committed, 2, "the whole transaction ran again");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(updated_at(&db, mine).await, t(100));
    assert_eq!(updated_at(&db, other).await, t(1));
}

#[tokio::test]
async fn gives_up_after_max_attempts() {
    let (db, _dir) = file_test_db(4).await;
    let (mine, other) = (chat(&db).await, chat(&db).await);
    let provider = DBProvider::<DomainError>::new(db.clone());
    let attempts = Arc::new(AtomicU32::new(0));

    let err = read_then_write(&provider, &db, &attempts, |_| true, (mine, other))
        .await
        .expect_err("every attempt is contended");

    assert!(err.is_contention(), "{err:?}");
    assert_eq!(attempts.load(Ordering::SeqCst), MAX_TX_ATTEMPTS);
    assert_eq!(
        updated_at(&db, mine).await,
        t(0),
        "nothing of `mine` committed"
    );
}

#[tokio::test]
async fn does_not_retry_domain_errors() {
    let db = test_db().await;
    let provider = DBProvider::<DomainError>::new(db);
    for err in [
        DomainError::ChatNotFound,
        DomainError::QuotaExceeded {
            scope: crate::domain::model::QuotaScope::Tokens,
        },
        // A domain error whose text looks like contention is still not one.
        DomainError::internal("error returned from database: (code: 5) database is locked"),
    ] {
        let expected = err.to_string();
        let err = std::sync::Mutex::new(Some(err));
        let attempts = AtomicU32::new(0);
        let res: Result<(), DomainError> = with_tx_retry(&provider, "test domain error", |_tx| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let e = err.lock().unwrap().take();
            Box::pin(async move { Err(e.unwrap_or(DomainError::MessageNotFound)) })
        })
        .await;
        assert_eq!(res.unwrap_err().to_string(), expected);
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "{expected}");
    }
}

#[tokio::test]
async fn database_contention_errors_are_classified() {
    let busy = sea_orm::DbErr::Exec(sea_orm::RuntimeErr::Internal(
        "error returned from database: (code: 517) database is locked".to_owned(),
    ));
    assert!(DomainError::from(toolkit_db::DbError::from(busy)).is_contention());
    let unique = sea_orm::DbErr::Exec(sea_orm::RuntimeErr::Internal(
        "error returned from database: (code: 2067) UNIQUE constraint failed".to_owned(),
    ));
    assert!(!DomainError::from(toolkit_db::DbError::from(unique)).is_contention());
}

// ---- outbox wakes across attempts ---------------------------------------------

/// Messages of the WARN events emitted on this thread.
#[derive(Clone, Default)]
struct Warnings(Arc<std::sync::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        if *event.metadata().level() == tracing::Level::WARN {
            let mut m = Message(String::new());
            event.record(&mut m);
            self.0.lock().unwrap().push(m.0);
        }
    }
}

/// Counts the messages delivered to its queue.
#[derive(Default)]
struct Delivered(AtomicU32);

#[async_trait::async_trait]
impl toolkit_db::outbox::LeasedMessageHandler for Delivered {
    async fn handle(
        &self,
        _msg: &toolkit_db::outbox::OutboxMessage,
    ) -> toolkit_db::outbox::MessageResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        toolkit_db::outbox::MessageResult::Ok
    }
}

#[tokio::test]
async fn failed_attempt_wakes_are_discarded_and_committed_wakes_fire() {
    use tracing_subscriber::layer::SubscriberExt;

    use crate::config::MiniChatConfig;
    use crate::infra::outbox::{
        OutboxEnqueuer, OutboxHandlers, PendingWakes, QueueKind, start_outbox,
    };

    let warnings = Warnings::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::Registry::default().with(warnings.clone()),
    );
    let db = test_db().await;
    let cfg = MiniChatConfig::default();
    let delivered = Arc::new(Delivered::default());
    let shared: Arc<dyn toolkit_db::outbox::LeasedMessageHandler> = delivered.clone();
    let handlers = OutboxHandlers::placeholders().map(|q, h| {
        if q == QueueKind::Usage {
            Arc::clone(&shared)
        } else {
            h
        }
    });
    let handle = start_outbox(db.clone(), &cfg, handlers).await.unwrap();
    let outbox = Arc::new(OutboxEnqueuer::new(cfg.outbox.clone()));
    outbox.set_outbox(Arc::clone(handle.outbox()));

    let provider = DBProvider::<DomainError>::new(db.clone());
    let attempts = Arc::new(AtomicU32::new(0));
    let (a, o) = (Arc::clone(&attempts), Arc::clone(&outbox));
    let wakes = with_tx_retry(&provider, "test wakes", move |tx| {
        let (a, o) = (Arc::clone(&a), Arc::clone(&o));
        Box::pin(async move {
            let mut wakes = PendingWakes::new();
            wakes.push(
                o.enqueue_json(tx, QueueKind::Usage, TENANT, "t", &serde_json::json!({}))
                    .await?,
            );
            if a.fetch_add(1, Ordering::SeqCst) == 0 {
                // The attempt fails after its enqueue: rolled back and retried.
                return Err(DomainError::DbContention("simulated".to_owned()));
            }
            Ok(wakes)
        })
    })
    .await
    .unwrap();
    wakes.fire();
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while delivered.0.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        delivered.0.load(Ordering::SeqCst),
        1,
        "only the committed row"
    );
    handle.stop().await;

    let warnings = warnings.0.lock().unwrap().clone();
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("Wake dropped unhandled")),
        "{warnings:?}"
    );
}
