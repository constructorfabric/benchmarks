//! Migrated `SQLite` test databases.
//!
//! [`test_db`] gives every caller its own **private-cache temp-file** database (WAL, busy
//! timeout) behind a **single pooled connection**.
//!
//! Why: the previous shared-cache in-memory database (4 connections) serialized access with
//! table-level locks that fail immediately with `SQLITE_LOCKED` ("database is deadlocked"), so a
//! finalization transaction racing the outbox sequencer flaked under load. A file database with
//! several connections is no better: a transaction that reads first and then writes (sea-orm
//! begins deferred transactions; the platform outbox's `register_queue` is one) fails at once
//! with `SQLITE_BUSY` / `SQLITE_BUSY_SNAPSHOT` when another connection writes in between, and
//! `busy_timeout` is not honoured for such an upgrade. Every `TestApp` start and many requests
//! therefore failed intermittently with "database is locked" (this is what broke the suites when
//! a WAL file database was first tried). With one connection the pool serializes all statements
//! and transactions, which removes the whole class; the unique indexes still enforce the
//! invariants the concurrency tests check. Code under test must not acquire a second connection
//! while it holds a transaction (it would wait for the pool); production uses PostgreSQL pools
//! where this does not apply.
//!
//! [`test_memory_db`] is the old shared-cache in-memory database for the few tests that cannot
//! keep a [`TestDb`] alive; it must not be used by tests that finalize turns or enqueue outbox
//! messages concurrently.

use std::ops::Deref;

use tempfile::TempDir;
use toolkit::contracts::DatabaseCapability as _;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, Db, connect_db};
use uuid::Uuid;

/// A migrated file database that is deleted when this value is dropped. Dereferences to [`Db`];
/// keep it alive for as long as the database (or any clone of the [`Db`]) is in use.
pub struct TestDb {
    db: Db,
    /// Removed on drop, after `db` (field order) closed its pool handle.
    _dir: TempDir,
}

impl TestDb {
    /// A cheap handle to the same database (the pool is shared).
    pub fn db(&self) -> Db {
        self.db.clone()
    }
}

impl Deref for TestDb {
    type Target = Db;

    fn deref(&self) -> &Db {
        &self.db
    }
}

/// Fresh migrated database in a private temp directory (WAL, 5 s busy timeout, one connection;
/// see the module docs).
///
/// # Panics
/// When the database cannot be created or migrated.
pub async fn test_db() -> TestDb {
    let dir = tempfile::tempdir().expect("create temp dir for the test database");
    let dsn = format!(
        "sqlite://{}?mode=rwc&journal_mode=WAL&busy_timeout=5000",
        dir.path().join("mini-chat.db").display()
    );
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect file sqlite");
    run_migrations_for_testing(&db, crate::gear::MiniChatGear::default().migrations())
        .await
        .expect("run migrations");
    TestDb { db, _dir: dir }
}

/// Fresh migrated shared-cache in-memory database (see the module docs for its limits). Needs no
/// cleanup, so it can be handed to code that outlives the caller's stack frame.
pub async fn test_memory_db() -> Db {
    let dsn = format!("sqlite:file:mc-{}?mode=memory&cache=shared", Uuid::new_v4());
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(4),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, crate::gear::MiniChatGear::default().migrations())
        .await
        .expect("run migrations");
    db
}
