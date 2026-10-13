#![allow(clippy::unwrap_used, clippy::expect_used, dead_code, unused_imports)]

//! Shared integration-test harness for the mini-chat gear.
//!
//! Each test binary uses a subset of these helpers, hence the lint allowances.
//!
//! DB part: a `SQLite` database file per call in its own temporary
//! directory, opened like the real server config (WAL, `busy_timeout` 5 s,
//! `synchronous = NORMAL`), migrated with the gear's `Migrator` plus the
//! platform outbox migrations, wrapped in a `DBProvider<DomainError>`. The
//! directory is removed when the returned [`TestDb`] is dropped.
//!
//! The pool has several connections, so the outbox pipeline (running for
//! the whole test) commits concurrently with request transactions, as on a
//! real server; gear transactions retry `SQLITE_BUSY_SNAPSHOT` through
//! `infra::db::tx::with_retry` (see `tests/db_retry.rs`).
//!
//! Tests that need raw SQL use [`test_db_with_raw`], which also returns a
//! plain `SeaORM` connection to the same file.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, DatabaseConnection, Statement};
use sea_orm_migration::MigratorTrait;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::secure::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use uuid::Uuid;

use mini_chat::domain::error::DomainError;
use mini_chat::infra::db::entity::{
    attachment, chat, chat_turn, chat_vector_store, message, message_attachment, message_reaction,
    quota_usage, thread_summary,
};
use mini_chat::infra::db::migrations::Migrator;

mod app;
mod attach;
mod fakes;
mod metrics;
mod stream;
pub use app::*;
pub use attach::*;
pub use fakes::*;
pub use metrics::*;
pub use stream::*;

/// All migrations the gear registers: mini-chat schema + default-prefix outbox.
fn all_migrations() -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
    let mut all = Migrator::migrations();
    all.extend(toolkit_db::outbox::outbox_migrations());
    all
}

/// Busy timeout of the test connections (the real server config's value).
const BUSY_TIMEOUT_MS: u32 = 5000;

/// A migrated per-test database; dereferences to its `DBProvider`. The
/// database files are deleted on drop.
pub struct TestDb {
    provider: Arc<DBProvider<DomainError>>,
    path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl TestDb {
    /// The provider (shared with the services under test).
    pub fn provider(&self) -> Arc<DBProvider<DomainError>> {
        Arc::clone(&self.provider)
    }

    /// A plain `SeaORM` connection to the same database file (raw SQL in
    /// tests only). sqlx's `SQLite` connections default to a 5 s busy
    /// timeout; the WAL journal mode is persistent in the file.
    pub async fn raw(&self) -> DatabaseConnection {
        let opts = sea_orm::ConnectOptions::new(format!("sqlite:{}", self.path.display()))
            .max_connections(2)
            .sqlx_logging(false)
            .to_owned();
        sea_orm::Database::connect(opts)
            .await
            .expect("raw connection to the test db")
    }
}

impl std::ops::Deref for TestDb {
    type Target = Arc<DBProvider<DomainError>>;

    fn deref(&self) -> &Self::Target {
        &self.provider
    }
}

/// Fresh migrated database file in a new temporary directory.
pub async fn test_db() -> TestDb {
    let dir = tempfile::Builder::new()
        .prefix("mini-chat-test-")
        .tempdir()
        .expect("temp dir for the test db");
    let path = dir.path().join("mini_chat.db");
    let dsn = format!(
        "sqlite:{}?mode=rwc&wal=true&synchronous=NORMAL&busy_timeout={BUSY_TIMEOUT_MS}",
        path.display()
    );
    let opts = ConnectOpts {
        max_conns: Some(4),
        min_conns: Some(1),
        ..Default::default()
    };
    let db = connect_db(&dsn, opts)
        .await
        .expect("connect sqlite test db");
    run_migrations_for_testing(&db, all_migrations())
        .await
        .expect("run migrations");
    TestDb {
        provider: Arc::new(DBProvider::new(db)),
        path,
        _dir: dir,
    }
}

/// Like [`test_db`], plus a raw `SeaORM` connection to the same database
/// (raw SQL is allowed in tests only).
pub async fn test_db_with_raw() -> (TestDb, DatabaseConnection) {
    let db = test_db().await;
    let raw = db.raw().await;
    (db, raw)
}

/// Run a raw statement (tests only).
pub async fn raw_exec(raw: &DatabaseConnection, sql: &str) -> Result<(), sea_orm::DbErr> {
    raw.execute_unprepared(sql).await.map(|_| ())
}

/// Query a single string column from every row (tests only).
pub async fn raw_strings(raw: &DatabaseConnection, sql: &str) -> Vec<String> {
    let rows = raw
        .query_all_raw(Statement::from_string(raw.get_database_backend(), sql))
        .await
        .expect("raw query");
    rows.iter()
        .map(|r| r.try_get_by_index::<String>(0).expect("string column"))
        .collect()
}

/// Tenant + owner scope, as the PEP compiles it for a chat owner.
pub fn tenant_scope(tenant: Uuid, user: Uuid) -> AccessScope {
    AccessScope::single(ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
        ScopeFilter::eq(pep_properties::OWNER_ID, user),
    ]))
}

/// Fixed, second-precision timestamp for fixtures.
pub fn ts(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("valid timestamp")
}

// ---------------------------------------------------------------------------
// Row fixtures (complete models; repos insert every column)
// ---------------------------------------------------------------------------

pub fn chat_row(tenant: Uuid, user: Uuid) -> chat::Model {
    chat::Model {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        user_id: user,
        model: "gpt-4.1".to_owned(),
        title: None,
        is_temporary: false,
        created_at: ts(1_700_000_000),
        updated_at: ts(1_700_000_000),
        deleted_at: None,
    }
}

pub fn turn_row(c: &chat::Model, request_id: Uuid, state: &str) -> chat_turn::Model {
    let terminal = state != "running";
    chat_turn::Model {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        chat_id: c.id,
        request_id,
        requester_type: "user".to_owned(),
        requester_user_id: Some(c.user_id),
        state: state.to_owned(),
        provider_name: None,
        provider_response_id: None,
        assistant_message_id: None,
        error_code: None,
        reserve_tokens: Some(1000),
        max_output_tokens_applied: Some(500),
        reserved_credits_micro: Some(10),
        policy_version_applied: Some(1),
        effective_model: Some("gpt-4.1".to_owned()),
        minimal_generation_floor_applied: Some(50),
        error_detail: None,
        deleted_at: None,
        replaced_by_request_id: None,
        started_at: ts(1_700_000_010),
        last_progress_at: Some(ts(1_700_000_010)),
        web_search_enabled: false,
        web_search_completed_count: 0,
        code_interpreter_completed_count: 0,
        file_search_completed_count: 0,
        completed_at: terminal.then(|| ts(1_700_000_020)),
        updated_at: ts(1_700_000_010),
    }
}

pub fn message_row(c: &chat::Model, request_id: Option<Uuid>, role: &str) -> message::Model {
    message::Model {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        chat_id: c.id,
        request_id,
        role: role.to_owned(),
        content: "hello".to_owned(),
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: None,
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: None,
        is_compressed: false,
        created_at: ts(1_700_000_030),
        deleted_at: None,
    }
}

pub fn attachment_row(c: &chat::Model) -> attachment::Model {
    attachment::Model {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        chat_id: c.id,
        uploaded_by_user_id: c.user_id,
        filename: "doc.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        size_bytes: 1024,
        storage_backend: "azure".to_owned(),
        provider_file_id: None,
        status: "pending".to_owned(),
        error_code: None,
        attachment_kind: "document".to_owned(),
        for_file_search: true,
        for_code_interpreter: false,
        doc_summary: None,
        img_thumbnail: None,
        img_thumbnail_width: None,
        img_thumbnail_height: None,
        summary_model: None,
        summary_updated_at: None,
        cleanup_status: None,
        cleanup_attempts: 0,
        last_cleanup_error: None,
        cleanup_updated_at: None,
        created_at: ts(1_700_000_040),
        updated_at: ts(1_700_000_040),
        deleted_at: None,
        secondary_file_id: None,
        secondary_status: "not_attempted".to_owned(),
        secondary_provider_kind: None,
    }
}

pub fn message_attachment_row(
    m: &message::Model,
    a: &attachment::Model,
) -> message_attachment::Model {
    message_attachment::Model {
        tenant_id: m.tenant_id,
        chat_id: m.chat_id,
        message_id: m.id,
        attachment_id: a.id,
        created_at: ts(1_700_000_050),
    }
}

pub fn thread_summary_row(c: &chat::Model, upto: &message::Model) -> thread_summary::Model {
    thread_summary::Model {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        chat_id: c.id,
        summary_text: "summary".to_owned(),
        summarized_up_to_created_at: upto.created_at,
        summarized_up_to_message_id: upto.id,
        token_estimate: 3,
        created_at: ts(1_700_000_060),
        updated_at: ts(1_700_000_060),
    }
}

pub fn vector_store_row(c: &chat::Model) -> chat_vector_store::Model {
    chat_vector_store::Model {
        id: Uuid::new_v4(),
        tenant_id: c.tenant_id,
        chat_id: c.id,
        vector_store_id: None,
        provider: "azure".to_owned(),
        file_count: 0,
        created_at: ts(1_700_000_070),
    }
}

pub fn quota_row(tenant: Uuid, user: Uuid, bucket: &str) -> quota_usage::Model {
    quota_usage::Model {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        user_id: user,
        period_type: "daily".to_owned(),
        period_start: time::Date::from_calendar_date(2026, time::Month::October, 4)
            .expect("valid date"),
        bucket: bucket.to_owned(),
        spent_credits_micro: 0,
        reserved_credits_micro: 0,
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        file_search_calls: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: ts(1_700_000_080),
    }
}

pub fn reaction_row(m: &message::Model, user: Uuid, reaction: &str) -> message_reaction::Model {
    message_reaction::Model {
        id: Uuid::new_v4(),
        message_id: m.id,
        user_id: user,
        tenant_id: m.tenant_id,
        reaction: reaction.to_owned(),
        created_at: ts(1_700_000_090),
    }
}
