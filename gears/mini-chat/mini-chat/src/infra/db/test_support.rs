#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `SQLite` test fixture: one shared in-memory database reachable through the
//! toolkit `Db` (Secure ORM) and through a raw connection (seeding and schema
//! introspection only).

use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, DbErr, Statement,
    Value,
};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, Db, connect_db};
use uuid::Uuid;

use crate::infra::db::migrations::Migrator;

/// Fixed timestamp used for every seeded row.
pub const TS: &str = "2026-01-01T00:00:00Z";

/// Column values of one row; setting a column twice keeps the last value.
#[derive(Clone, Default)]
pub struct Row(Vec<(String, Value)>);

impl Row {
    /// Sets `column` to `value`.
    #[must_use]
    pub fn set(mut self, column: &str, value: impl Into<Value>) -> Self {
        self.0.retain(|(c, _)| c != column);
        self.0.push((column.to_owned(), value.into()));
        self
    }

    /// Sets `column` to SQL `NULL`.
    #[must_use]
    pub fn null(self, column: &str) -> Self {
        self.set(column, Value::String(None))
    }

    /// Removes `column` so that the database default applies.
    #[must_use]
    pub fn unset(mut self, column: &str) -> Self {
        self.0.retain(|(c, _)| c != column);
        self
    }
}

pub fn chat_row(id: Uuid, tenant: Uuid, user: Uuid) -> Row {
    Row::default()
        .set("id", id)
        .set("tenant_id", tenant)
        .set("user_id", user)
        .set("model", "gpt-5.2")
        .set("is_temporary", false)
        .set("created_at", TS)
        .set("updated_at", TS)
}

pub fn message_row(id: Uuid, tenant: Uuid, chat: Uuid) -> Row {
    Row::default()
        .set("id", id)
        .set("tenant_id", tenant)
        .set("chat_id", chat)
        .set("role", "user")
        .set("content", "hello")
        .set("content_type", "text")
        .set("token_estimate", 0_i64)
        .set("request_kind", "chat")
        .set("is_compressed", false)
        .set("created_at", TS)
}

pub fn turn_row(id: Uuid, tenant: Uuid, chat: Uuid, request_id: Uuid, state: &str) -> Row {
    let row = Row::default()
        .set("id", id)
        .set("tenant_id", tenant)
        .set("chat_id", chat)
        .set("request_id", request_id)
        .set("requester_type", "user")
        .set("requester_user_id", Uuid::from_u128(0xAA))
        .set("state", state)
        .set("started_at", TS)
        .set("updated_at", TS);
    if state == "running" {
        row.set("last_progress_at", TS)
    } else {
        row.set("completed_at", TS)
    }
}

pub fn attachment_row(id: Uuid, tenant: Uuid, chat: Uuid) -> Row {
    Row::default()
        .set("id", id)
        .set("tenant_id", tenant)
        .set("chat_id", chat)
        .set("uploaded_by_user_id", Uuid::from_u128(0xAA))
        .set("filename", "a.txt")
        .set("content_type", "text/plain")
        .set("size_bytes", 10_i64)
        .set("status", "ready")
        .set("attachment_kind", "document")
        .set("for_file_search", true)
        .set("for_code_interpreter", false)
        .set("created_at", TS)
        .set("updated_at", TS)
}

/// A temporary directory removed on drop.
pub struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("mini_chat_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create the temp dir");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}

pub struct TestDb {
    /// Toolkit handle (Secure ORM).
    pub db: Db,
    /// Raw connection for seeding and introspection.
    pub raw: DatabaseConnection,
    /// Directory of a file-backed database (removed with the fixture).
    _dir: Option<TempDir>,
}

impl TestDb {
    /// Connects to a fresh shared in-memory database without any schema.
    pub async fn empty() -> Self {
        let dsn = format!(
            "sqlite:file:mini_chat_{}?mode=memory&cache=shared",
            Uuid::new_v4()
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
        .expect("connect sqlite");
        let mut options = ConnectOptions::new(dsn);
        options.max_connections(1).min_connections(1);
        let raw = Database::connect(options).await.expect("connect raw");
        Self {
            db,
            raw,
            _dir: None,
        }
    }

    /// Connects to a fresh file-backed database the way the server does: WAL,
    /// a busy timeout and a pool of several connections, so that concurrent
    /// transactions really run on different connections and contend for the
    /// file lock (the shared in-memory fixture serializes them on its single
    /// connection).
    pub async fn empty_file() -> Self {
        let dir = TempDir::new();
        let dsn = format!(
            "sqlite://{}?mode=rwc&WAL=true&synchronous=NORMAL&busy_timeout=5000",
            dir.0.join("mini_chat.db").display()
        );
        let db = connect_db(
            &dsn,
            ConnectOpts {
                max_conns: Some(5),
                ..Default::default()
            },
        )
        .await
        .expect("connect file sqlite");
        let mut options = ConnectOptions::new(format!(
            "sqlite://{}?mode=rwc",
            dir.0.join("mini_chat.db").display()
        ));
        options.max_connections(1).min_connections(1);
        let raw = Database::connect(options).await.expect("connect raw");
        Self {
            db,
            raw,
            _dir: Some(dir),
        }
    }

    /// [`Self::empty_file`] with the gear migrations applied.
    pub async fn migrated_file() -> Self {
        let this = Self::empty_file().await;
        run_migrations_for_testing(&this.db, Migrator::migrations())
            .await
            .expect("run migrations");
        this
    }

    /// Connects and applies the gear migrations.
    pub async fn migrated() -> Self {
        let this = Self::empty().await;
        run_migrations_for_testing(&this.db, Migrator::migrations())
            .await
            .expect("run migrations");
        this
    }

    /// Inserts `row` into `table`.
    pub async fn insert(&self, table: &str, row: Row) -> Result<(), DbErr> {
        let columns: Vec<&str> = row.0.iter().map(|(c, _)| c.as_str()).collect();
        let placeholders = vec!["?"; columns.len()].join(", ");
        let sql = format!(
            "INSERT INTO {table} ({}) VALUES ({placeholders})",
            columns.join(", ")
        );
        let values: Vec<Value> = row.0.into_iter().map(|(_, v)| v).collect();
        self.raw
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                sql,
                values,
            ))
            .await
            .map(|_| ())
    }

    /// Soft-deletes the row `id` of `table`.
    pub async fn soft_delete(&self, table: &str, id: Uuid) {
        self.raw
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("UPDATE {table} SET deleted_at = ? WHERE id = ?"),
                [Value::from(TS), Value::from(id)],
            ))
            .await
            .expect("soft delete");
    }

    /// Names of all tables of the database.
    pub async fn table_names(&self) -> Vec<String> {
        let rows = self
            .raw
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'table'".to_owned(),
            ))
            .await
            .expect("list tables");
        rows.iter()
            .map(|r| r.try_get::<String>("", "name").expect("name column"))
            .collect()
    }

    /// Number of rows of `table`.
    pub async fn count(&self, table: &str) -> i64 {
        let row = self
            .raw
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT COUNT(*) AS n FROM {table}"),
            ))
            .await
            .expect("count")
            .expect("one row");
        row.try_get::<i64>("", "n").expect("n column")
    }

    /// First column of the first row of `sql` as an integer.
    pub async fn query_i64(&self, sql: &str) -> i64 {
        let row = self
            .raw
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .expect("query")
            .expect("one row");
        row.try_get_by_index::<i64>(0).expect("integer column")
    }

    /// First column of the first row of `sql` as text.
    pub async fn query_string(&self, sql: &str) -> String {
        let row = self
            .raw
            .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
            .await
            .expect("query")
            .expect("one row");
        row.try_get_by_index::<String>(0).expect("text column")
    }

    /// Deletes the row `id` of `table` (hard delete).
    pub async fn delete_by_id(&self, table: &str, id: Uuid) -> Result<(), DbErr> {
        self.raw
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("DELETE FROM {table} WHERE id = ?"),
                [Value::from(id)],
            ))
            .await
            .map(|_| ())
    }
}
