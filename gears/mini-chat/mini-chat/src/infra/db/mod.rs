//! Database layer: migrations and `SeaORM` entities (DESIGN §3.7).

pub mod entities;
pub mod migrations;
pub mod repos;
pub mod tx;

use sea_orm_migration::{MigrationTrait, MigratorTrait};
use toolkit_db::Db;

/// Table prefix of the shared outbox tables (also passed to the outbox builder).
pub const OUTBOX_TABLE_PREFIX: &str = "mini_chat_outbox";

/// All migrations of the gear: its own schema followed by the platform outbox
/// migrations with [`OUTBOX_TABLE_PREFIX`].
///
/// # Panics
///
/// Panics if [`OUTBOX_TABLE_PREFIX`] is not a valid outbox prefix (a constant,
/// so this is a programming error caught by the unit tests).
#[must_use]
pub fn all_migrations() -> Vec<Box<dyn MigrationTrait>> {
    let mut migrations = migrations::Migrator::migrations();
    match toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX) {
        Ok(outbox) => migrations.extend(outbox),
        Err(e) => {
            panic!("mini-chat outbox migration prefix '{OUTBOX_TABLE_PREFIX}' is invalid: {e}")
        }
    }
    migrations
}

/// Fresh, isolated in-memory `SQLite` database (single connection) with
/// [`all_migrations`] applied. Returns the secure `toolkit_db::Db` handle that
/// the repositories and `DBProvider` are built from.
///
/// Test support only (used by unit tests and `mini_chat::testing`).
///
/// # Panics
///
/// Panics if the database cannot be created or migrated.
#[doc(hidden)]
pub async fn test_db() -> Db {
    connect_migrated(&test_dsn()).await
}

/// Unique shared-cache in-memory `SQLite` DSN.
pub(crate) fn test_dsn() -> String {
    format!(
        "sqlite:file:mc_{}?mode=memory&cache=shared",
        uuid::Uuid::new_v4().simple()
    )
}

/// Connect to `dsn` (single connection) and apply [`all_migrations`].
pub(crate) async fn connect_migrated(dsn: &str) -> Db {
    connect_migrated_with(dsn, 1).await
}

/// Temporary directory of a [`file_test_db`]; removed on drop.
#[doc(hidden)]
#[derive(Debug)]
pub struct TestDbDir(std::path::PathBuf);

impl Drop for TestDbDir {
    fn drop(&mut self) {
        _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Fresh file-backed `SQLite` database in a temporary directory, in WAL mode
/// with a pool of `max_conns` connections (like the real server), with
/// [`all_migrations`] applied. Unlike [`test_db`], concurrent transactions run
/// on different connections, so database contention (`SQLITE_BUSY`,
/// `SQLITE_BUSY_SNAPSHOT`) reproduces. Keep the returned directory guard alive
/// as long as the database is used.
///
/// Test support only (used by unit tests and `mini_chat::testing`).
///
/// # Panics
///
/// Panics if the directory or the database cannot be created or migrated.
#[doc(hidden)]
#[allow(clippy::expect_used)]
pub async fn file_test_db(max_conns: u32) -> (Db, TestDbDir) {
    let dir =
        std::env::temp_dir().join(format!("mini-chat-test-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("create the test database directory");
    let dir = TestDbDir(dir);
    let dsn = format!(
        "sqlite://{}?mode=rwc&wal=true&synchronous=NORMAL&busy_timeout=5000",
        dir.0.join("mini-chat.db").display()
    );
    (connect_migrated_with(&dsn, max_conns).await, dir)
}

/// Connect to `dsn` with a pool of `max_conns` and apply [`all_migrations`].
#[allow(clippy::expect_used)]
async fn connect_migrated_with(dsn: &str, max_conns: u32) -> Db {
    let db = toolkit_db::connect_db(
        dsn,
        toolkit_db::ConnectOpts {
            max_conns: Some(max_conns),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect test sqlite");
    toolkit_db::migration_runner::run_migrations_for_testing(&db, all_migrations())
        .await
        .expect("run mini-chat migrations");
    db
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod migrations_tests;
