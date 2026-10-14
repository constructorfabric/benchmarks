//! Schema-level tests of the P1 migrations against in-memory SQLite: the nine tables, every
//! UNIQUE, CHECK, FOREIGN KEY and partial index of DESIGN 3.7, the constraints that are
//! deliberately not enforced (ADR-0010), the absent MCP and `tenant_closure` tables, and the
//! platform outbox tables.
//!
//! Slice S1, algorithm `schema-migration`, DoD `schema` and `deferred-scope`. PostgreSQL
//! behavior is not reachable from SQLite; the per-dialect statement lists are the author
//! stage's responsibility.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::doc_markdown)]

use std::collections::BTreeSet;

use mini_chat::infra::storage::Migrator;
use mini_chat::infra::storage::migrations::all_migrations;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, Statement};
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, connect_db};
use uuid::Uuid;

const NINE_TABLES: [&str; 9] = [
    "chats",
    "messages",
    "chat_turns",
    "attachments",
    "message_attachments",
    "thread_summaries",
    "chat_vector_stores",
    "quota_usage",
    "message_reactions",
];

const TS: &str = "'2026-01-01 00:00:00+00:00'";

// ── harness ──────────────────────────────────────────────────────────────────

fn stmt(db: &DatabaseConnection, sql: impl Into<String>) -> Statement {
    Statement::from_string(db.get_database_backend(), sql.into())
}

async fn exec(db: &DatabaseConnection, sql: impl Into<String>) -> Result<(), DbErr> {
    db.execute_raw(stmt(db, sql)).await.map(|_| ())
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(stmt(db, sql))
        .await
        .expect("query")
        .expect("one row")
        .try_get_by_index::<i64>(0)
        .expect("i64")
}

async fn text(db: &DatabaseConnection, sql: &str) -> String {
    db.query_one_raw(stmt(db, sql))
        .await
        .expect("query")
        .expect("one row")
        .try_get_by_index::<String>(0)
        .expect("string")
}

/// A fresh in-memory SQLite with foreign keys enforced and the gear migrations applied.
async fn migrated() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:")
        .await
        .expect("connect in-memory sqlite");
    exec(&db, "PRAGMA foreign_keys = ON;")
        .await
        .expect("enable foreign keys");
    Migrator::up(&db, None)
        .await
        .expect("apply the gear migrations");
    db
}

async fn table_names(db: &DatabaseConnection) -> BTreeSet<String> {
    let rows = db
        .query_all_raw(stmt(
            db,
            "SELECT name FROM sqlite_master WHERE type = 'table'",
        ))
        .await
        .expect("list tables");
    rows.iter()
        .map(|row| row.try_get_by_index::<String>(0).unwrap())
        .collect()
}

/// UUID bytes as an SQL blob literal (the stored form: 16 bytes, not text).
fn id(n: u128) -> String {
    format!("x'{n:032x}'")
}

/// Ordered column assignments of one INSERT.
#[derive(Clone)]
struct Row(Vec<(String, String)>);

impl Row {
    fn of(cols: &[(&str, &str)]) -> Self {
        Self(
            cols.iter()
                .map(|(c, v)| ((*c).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    fn set(mut self, col: &str, value: &str) -> Self {
        self.0.retain(|(c, _)| c != col);
        self.0.push((col.to_owned(), value.to_owned()));
        self
    }

    fn without(mut self, col: &str) -> Self {
        self.0.retain(|(c, _)| c != col);
        self
    }

    fn sql(&self, table: &str) -> String {
        let cols: Vec<&str> = self.0.iter().map(|(c, _)| c.as_str()).collect();
        let vals: Vec<&str> = self.0.iter().map(|(_, v)| v.as_str()).collect();
        format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            cols.join(", "),
            vals.join(", ")
        )
    }
}

async fn insert(db: &DatabaseConnection, table: &str, row: &Row) -> Result<(), DbErr> {
    exec(db, row.sql(table)).await
}

async fn insert_ok(db: &DatabaseConnection, table: &str, row: &Row) {
    insert(db, table, row)
        .await
        .unwrap_or_else(|e| panic!("insert into {table} must succeed: {e}"));
}

async fn insert_err(db: &DatabaseConnection, table: &str, row: &Row, why: &str) {
    assert!(
        insert(db, table, row).await.is_err(),
        "insert into {table} must fail: {why}"
    );
}

fn chat_row(n: u128) -> Row {
    Row::of(&[
        ("id", &id(n)),
        ("tenant_id", &id(0xA)),
        ("user_id", &id(0xB)),
        ("model", "'gpt-5.2'"),
        ("title", "'t'"),
        ("is_temporary", "0"),
        ("created_at", TS),
        ("updated_at", TS),
    ])
}

fn message_row(n: u128, chat: u128) -> Row {
    Row::of(&[
        ("id", &id(n)),
        ("tenant_id", &id(0xA)),
        ("chat_id", &id(chat)),
        ("role", "'user'"),
        ("content", "'hello'"),
        ("content_type", "'text'"),
        ("token_estimate", "0"),
        ("request_kind", "'chat'"),
        ("is_compressed", "0"),
        ("created_at", TS),
    ])
}

fn turn_row(n: u128, chat: u128, request: u128) -> Row {
    Row::of(&[
        ("id", &id(n)),
        ("tenant_id", &id(0xA)),
        ("chat_id", &id(chat)),
        ("request_id", &id(request)),
        ("requester_type", "'user'"),
        ("state", "'completed'"),
        ("started_at", TS),
        ("completed_at", TS),
        ("updated_at", TS),
    ])
}

fn attachment_row(n: u128, chat: u128) -> Row {
    Row::of(&[
        ("id", &id(n)),
        ("tenant_id", &id(0xA)),
        ("chat_id", &id(chat)),
        ("uploaded_by_user_id", &id(0xB)),
        ("filename", "'a.txt'"),
        ("status", "'ready'"),
        ("attachment_kind", "'document'"),
        ("created_at", TS),
    ])
}

fn quota_row(n: u128, bucket: &str) -> Row {
    Row::of(&[
        ("id", &id(n)),
        ("tenant_id", &id(0xA)),
        ("user_id", &id(0xB)),
        ("period_type", "'daily'"),
        ("period_start", "'2026-01-01'"),
        ("bucket", bucket),
        ("updated_at", TS),
    ])
}

fn reaction_row(n: u128, message: u128, user: u128) -> Row {
    Row::of(&[
        ("id", &id(n)),
        ("message_id", &id(message)),
        ("user_id", &id(user)),
        ("tenant_id", &id(0xA)),
        ("reaction", "'like'"),
        ("created_at", TS),
    ])
}

// ── shape ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_migration_creates_the_nine_p1_tables() {
    let db = migrated().await;
    let tables = table_names(&db).await;
    for table in NINE_TABLES {
        assert!(
            tables.contains(table),
            "missing table {table}; have {tables:?}"
        );
    }
}

#[tokio::test]
async fn no_mcp_tenant_closure_or_policy_snapshot_table_is_created() {
    let db = migrated().await;
    let tables = table_names(&db).await;
    for banned in [
        "mcp_servers",
        "mcp_server_tools",
        "role_mcp_servers",
        "tenant_closure",
    ] {
        assert!(!tables.contains(banned), "{banned} must not exist");
    }
    for table in &tables {
        assert!(!table.contains("mcp"), "{table}");
        assert!(!table.contains("closure"), "{table}");
        assert!(
            !table.contains("snapshot"),
            "{table}: the gear keeps no policy snapshot table"
        );
    }
}

#[tokio::test]
async fn the_gear_migrations_create_exactly_the_nine_tables_plus_the_migration_history() {
    let db = migrated().await;
    let tables: BTreeSet<String> = table_names(&db)
        .await
        .into_iter()
        .filter(|t| !t.starts_with("seaql_") && !t.starts_with("sqlite_"))
        .collect();
    let expected: BTreeSet<String> = NINE_TABLES.iter().map(|t| (*t).to_owned()).collect();
    assert_eq!(tables, expected);
}

#[tokio::test]
async fn uuid_columns_are_declared_text_in_sqlite() {
    let db = migrated().await;
    let uuid_columns = [
        ("chats", "id"),
        ("chats", "tenant_id"),
        ("chats", "user_id"),
        ("messages", "id"),
        ("messages", "chat_id"),
        ("messages", "request_id"),
        ("chat_turns", "id"),
        ("chat_turns", "assistant_message_id"),
        ("attachments", "uploaded_by_user_id"),
        ("message_attachments", "message_id"),
        ("thread_summaries", "summarized_up_to_message_id"),
        ("quota_usage", "user_id"),
        ("message_reactions", "message_id"),
    ];
    for (table, column) in uuid_columns {
        let declared = text(
            &db,
            &format!("SELECT type FROM pragma_table_info('{table}') WHERE name = '{column}'"),
        )
        .await;
        assert_eq!(declared.to_uppercase(), "TEXT", "{table}.{column}");
    }
}

// ── primary keys, NOT NULL and nullability ───────────────────────────────────

#[tokio::test]
async fn primary_keys_match_the_design() {
    let db = migrated().await;
    for table in NINE_TABLES.iter().filter(|t| **t != "message_attachments") {
        let pk = text(
            &db,
            &format!("SELECT name FROM pragma_table_info('{table}') WHERE pk = 1"),
        )
        .await;
        assert_eq!(pk, "id", "{table}");
        let count = scalar(
            &db,
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE pk > 0"),
        )
        .await;
        assert_eq!(count, 1, "{table} has a single-column primary key");
    }
    for (position, column) in [(1, "chat_id"), (2, "message_id"), (3, "attachment_id")] {
        let name = text(
            &db,
            &format!(
                "SELECT name FROM pragma_table_info('message_attachments') WHERE pk = {position}"
            ),
        )
        .await;
        assert_eq!(name, column);
    }
}

#[tokio::test]
async fn documented_not_null_columns_are_not_null() {
    let db = migrated().await;
    let not_null: [(&str, &[&str]); 9] = [
        (
            "chats",
            &["tenant_id", "user_id", "created_at", "updated_at"],
        ),
        (
            "messages",
            &[
                "tenant_id",
                "chat_id",
                "role",
                "content",
                "content_type",
                "created_at",
            ],
        ),
        (
            "chat_turns",
            &[
                "tenant_id",
                "chat_id",
                "request_id",
                "requester_type",
                "state",
                "started_at",
            ],
        ),
        (
            "attachments",
            &[
                "tenant_id",
                "chat_id",
                "uploaded_by_user_id",
                "filename",
                "status",
                "attachment_kind",
                "storage_backend",
                "created_at",
            ],
        ),
        (
            "message_attachments",
            &["tenant_id", "chat_id", "created_at"],
        ),
        (
            "thread_summaries",
            &[
                "tenant_id",
                "chat_id",
                "summarized_up_to_created_at",
                "summarized_up_to_message_id",
                "created_at",
                "updated_at",
            ],
        ),
        (
            "chat_vector_stores",
            &[
                "tenant_id",
                "chat_id",
                "provider",
                "created_at",
                "file_count",
            ],
        ),
        (
            "quota_usage",
            &[
                "tenant_id",
                "user_id",
                "period_type",
                "period_start",
                "bucket",
            ],
        ),
        (
            "message_reactions",
            &[
                "message_id",
                "user_id",
                "tenant_id",
                "reaction",
                "created_at",
            ],
        ),
    ];
    for (table, columns) in not_null {
        for column in columns {
            let flag = scalar(
                &db,
                &format!(
                    "SELECT \"notnull\" FROM pragma_table_info('{table}') WHERE name = '{column}'"
                ),
            )
            .await;
            assert_eq!(flag, 1, "{table}.{column} must be NOT NULL");
        }
    }
}

#[tokio::test]
async fn documented_nullable_columns_are_nullable() {
    let db = migrated().await;
    let nullable: [(&str, &[&str]); 5] = [
        ("chats", &["deleted_at"]),
        (
            "messages",
            &["request_id", "provider_response_id", "deleted_at"],
        ),
        (
            "chat_turns",
            &[
                "requester_user_id",
                "reserve_tokens",
                "max_output_tokens_applied",
                "reserved_credits_micro",
                "policy_version_applied",
                "effective_model",
                "minimal_generation_floor_applied",
                "last_progress_at",
                "completed_at",
                "deleted_at",
            ],
        ),
        (
            "attachments",
            &[
                "provider_file_id",
                "cleanup_status",
                "deleted_at",
                "secondary_provider_kind",
            ],
        ),
        ("chat_vector_stores", &["vector_store_id"]),
    ];
    for (table, columns) in nullable {
        for column in columns {
            let flag = scalar(
                &db,
                &format!(
                    "SELECT \"notnull\" FROM pragma_table_info('{table}') WHERE name = '{column}'"
                ),
            )
            .await;
            assert_eq!(flag, 0, "{table}.{column} must be nullable");
        }
    }
}

#[tokio::test]
async fn message_reactions_have_no_chat_id_column() {
    let db = migrated().await;
    let found = scalar(
        &db,
        "SELECT COUNT(*) FROM pragma_table_info('message_reactions') WHERE name = 'chat_id'",
    )
    .await;
    assert_eq!(found, 0);
}

#[tokio::test]
async fn chats_reject_null_in_every_not_null_column() {
    let db = migrated().await;
    for column in ["tenant_id", "user_id", "created_at", "updated_at"] {
        insert_err(&db, "chats", &chat_row(1).set(column, "NULL"), column).await;
    }
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "chats", &chat_row(2).set("deleted_at", "NULL")).await;
}

// ── defaults ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn messages_default_token_columns_to_zero_and_features_to_an_empty_array() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "messages", &message_row(10, 1)).await;
    for column in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "reasoning_tokens",
    ] {
        let value = scalar(&db, &format!("SELECT {column} FROM messages")).await;
        assert_eq!(value, 0, "{column}");
    }
    assert_eq!(text(&db, "SELECT features_used FROM messages").await, "[]");
}

#[tokio::test]
async fn chat_turn_counters_default_to_zero_and_web_search_enabled_to_false() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "chat_turns", &turn_row(20, 1, 100)).await;
    for column in [
        "web_search_completed_count",
        "code_interpreter_completed_count",
        "file_search_completed_count",
        "web_search_enabled",
    ] {
        assert_eq!(
            scalar(&db, &format!("SELECT {column} FROM chat_turns")).await,
            0,
            "{column}"
        );
    }
}

#[tokio::test]
async fn chat_turn_started_at_has_no_database_default() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_err(
        &db,
        "chat_turns",
        &turn_row(20, 1, 100).without("started_at"),
        "started_at is set by the application clock",
    )
    .await;
}

#[tokio::test]
async fn attachment_defaults_apply() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(
        &db,
        "attachments",
        &attachment_row(30, 1).without("storage_backend"),
    )
    .await;
    assert_eq!(
        text(&db, "SELECT storage_backend FROM attachments").await,
        "azure"
    );
    assert_eq!(
        text(&db, "SELECT secondary_status FROM attachments").await,
        "not_attempted"
    );
    assert_eq!(
        scalar(&db, "SELECT cleanup_attempts FROM attachments").await,
        0
    );
    assert_eq!(
        scalar(&db, "SELECT for_file_search FROM attachments").await,
        0
    );
    assert_eq!(
        scalar(&db, "SELECT for_code_interpreter FROM attachments").await,
        0
    );
    assert_eq!(
        scalar(&db, "SELECT updated_at IS NOT NULL FROM attachments").await,
        1
    );
}

#[tokio::test]
async fn quota_usage_counters_default_to_zero_including_the_reserved_columns() {
    let db = migrated().await;
    insert_ok(&db, "quota_usage", &quota_row(40, "'total'")).await;
    for column in [
        "spent_credits_micro",
        "reserved_credits_micro",
        "calls",
        "input_tokens",
        "output_tokens",
        "file_search_calls",
        "web_search_calls",
        "code_interpreter_calls",
        "rag_retrieval_calls",
        "image_inputs",
        "image_upload_bytes",
    ] {
        assert_eq!(
            scalar(&db, &format!("SELECT {column} FROM quota_usage")).await,
            0,
            "{column}"
        );
    }
}

// ── UNIQUE constraints ───────────────────────────────────────────────────────

#[tokio::test]
async fn messages_are_unique_per_chat_request_and_role_among_live_rows_with_a_request_id() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    let with_request = |n: u128, role: &str| {
        message_row(n, 1)
            .set("request_id", &id(500))
            .set("role", &format!("'{role}'"))
    };

    insert_ok(&db, "messages", &with_request(10, "user")).await;
    insert_err(
        &db,
        "messages",
        &with_request(11, "user"),
        "same chat, request and role",
    )
    .await;
    insert_ok(&db, "messages", &with_request(12, "assistant")).await;

    // NULL request ids never collide
    insert_ok(
        &db,
        "messages",
        &message_row(13, 1).set("request_id", "NULL"),
    )
    .await;
    insert_ok(
        &db,
        "messages",
        &message_row(14, 1).set("request_id", "NULL"),
    )
    .await;

    // a soft-deleted row does not block a new live one, and is not blocked itself
    exec(
        &db,
        format!(
            "UPDATE messages SET deleted_at = {TS} WHERE id = {}",
            id(10)
        ),
    )
    .await
    .unwrap();
    insert_ok(&db, "messages", &with_request(15, "user")).await;
    insert_ok(
        &db,
        "messages",
        &with_request(16, "user").set("deleted_at", TS),
    )
    .await;
}

#[tokio::test]
async fn chat_turns_are_unique_per_chat_and_request() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "chats", &chat_row(2)).await;
    insert_ok(&db, "chat_turns", &turn_row(20, 1, 100)).await;
    insert_err(
        &db,
        "chat_turns",
        &turn_row(21, 1, 100),
        "same (chat_id, request_id)",
    )
    .await;
    insert_ok(&db, "chat_turns", &turn_row(22, 2, 100)).await;
    insert_ok(&db, "chat_turns", &turn_row(23, 1, 101)).await;
}

#[tokio::test]
async fn at_most_one_live_running_turn_per_chat() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    let running = |n: u128, request: u128| {
        turn_row(n, 1, request)
            .set("state", "'running'")
            .set("completed_at", "NULL")
            .set("last_progress_at", TS)
    };
    insert_ok(&db, "chat_turns", &running(20, 100)).await;
    insert_err(
        &db,
        "chat_turns",
        &running(21, 101),
        "second running turn in the chat",
    )
    .await;
    insert_ok(&db, "chat_turns", &turn_row(22, 1, 102)).await;

    // a soft-deleted running turn does not count
    exec(
        &db,
        format!(
            "UPDATE chat_turns SET deleted_at = {TS} WHERE id = {}",
            id(20)
        ),
    )
    .await
    .unwrap();
    insert_ok(&db, "chat_turns", &running(23, 103)).await;
}

#[tokio::test]
async fn thread_summaries_are_one_per_chat() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    let summary = |n: u128| {
        Row::of(&[
            ("id", &id(n)),
            ("tenant_id", &id(0xA)),
            ("chat_id", &id(1)),
            ("summary_text", "'s'"),
            ("summarized_up_to_created_at", TS),
            ("summarized_up_to_message_id", &id(10)),
            ("token_estimate", "1"),
            ("created_at", TS),
            ("updated_at", TS),
        ])
    };
    insert_ok(&db, "thread_summaries", &summary(60)).await;
    insert_err(
        &db,
        "thread_summaries",
        &summary(61),
        "second summary for the chat",
    )
    .await;
    for column in [
        "summarized_up_to_created_at",
        "summarized_up_to_message_id",
        "created_at",
        "updated_at",
    ] {
        let mut db_row = summary(62);
        db_row = db_row.set(column, "NULL");
        insert_err(&db, "thread_summaries", &db_row, column).await;
    }
}

#[tokio::test]
async fn chat_vector_stores_are_unique_per_tenant_and_chat_and_vector_store_id_is_nullable() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    let store = |n: u128, tenant: u128| {
        Row::of(&[
            ("id", &id(n)),
            ("tenant_id", &id(tenant)),
            ("chat_id", &id(1)),
            ("vector_store_id", "NULL"),
            ("provider", "'openai'"),
            ("file_count", "0"),
            ("created_at", TS),
        ])
    };
    insert_ok(&db, "chat_vector_stores", &store(70, 0xA)).await;
    insert_err(
        &db,
        "chat_vector_stores",
        &store(71, 0xA),
        "same (tenant_id, chat_id)",
    )
    .await;
    insert_ok(&db, "chat_vector_stores", &store(72, 0xB)).await;
    insert_err(
        &db,
        "chat_vector_stores",
        &store(73, 0xC).set("file_count", "-1"),
        "file_count >= 0",
    )
    .await;
    insert_err(
        &db,
        "chat_vector_stores",
        &store(74, 0xD).without("provider"),
        "provider NOT NULL",
    )
    .await;
}

#[tokio::test]
async fn quota_usage_is_unique_per_user_period_and_bucket() {
    let db = migrated().await;
    insert_ok(&db, "quota_usage", &quota_row(40, "'total'")).await;
    insert_err(
        &db,
        "quota_usage",
        &quota_row(41, "'total'"),
        "same bucket row",
    )
    .await;
    insert_ok(&db, "quota_usage", &quota_row(42, "'tier:premium'")).await;
    insert_ok(
        &db,
        "quota_usage",
        &quota_row(43, "'total'").set("period_type", "'monthly'"),
    )
    .await;
    insert_ok(
        &db,
        "quota_usage",
        &quota_row(44, "'total'").set("period_start", "'2026-01-02'"),
    )
    .await;
    insert_ok(
        &db,
        "quota_usage",
        &quota_row(45, "'total'").set("user_id", &id(0xC)),
    )
    .await;
    insert_ok(
        &db,
        "quota_usage",
        &quota_row(46, "'total'").set("tenant_id", &id(0xD)),
    )
    .await;
}

#[tokio::test]
async fn message_reactions_are_unique_per_message_and_user_and_limited_to_like_and_dislike() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "messages", &message_row(10, 1)).await;
    insert_ok(&db, "message_reactions", &reaction_row(80, 10, 0xB)).await;
    insert_err(
        &db,
        "message_reactions",
        &reaction_row(81, 10, 0xB),
        "same (message_id, user_id)",
    )
    .await;
    insert_ok(&db, "message_reactions", &reaction_row(82, 10, 0xC)).await;
    insert_ok(
        &db,
        "message_reactions",
        &reaction_row(83, 10, 0xD).set("reaction", "'dislike'"),
    )
    .await;
    insert_err(
        &db,
        "message_reactions",
        &reaction_row(84, 10, 0xE).set("reaction", "'love'"),
        "reaction CHECK",
    )
    .await;
    insert_err(
        &db,
        "message_reactions",
        &reaction_row(85, 999, 0xF),
        "message_id foreign key",
    )
    .await;
}

// ── enforced CHECK constraints ───────────────────────────────────────────────

#[tokio::test]
async fn chat_turn_value_checks_are_enforced() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    for (n, requester) in [(20u128, "'user'"), (21, "'system'")] {
        insert_ok(
            &db,
            "chat_turns",
            &turn_row(n, 1, 100 + n).set("requester_type", requester),
        )
        .await;
    }
    insert_err(
        &db,
        "chat_turns",
        &turn_row(22, 1, 122).set("requester_type", "'robot'"),
        "requester_type",
    )
    .await;
    for (n, state) in [
        (30u128, "'running'"),
        (31, "'completed'"),
        (32, "'failed'"),
        (33, "'cancelled'"),
    ] {
        let row = turn_row(n, 1, 100 + n)
            .set("state", state)
            .set("last_progress_at", TS);
        insert_ok(&db, "chat_turns", &row).await;
    }
    insert_err(
        &db,
        "chat_turns",
        &turn_row(34, 1, 134).set("state", "'queued'"),
        "state",
    )
    .await;
}

#[tokio::test]
async fn attachment_value_checks_are_enforced() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    let mut n = 300u128;
    let mut next = || {
        n += 1;
        n
    };
    for kind in ["'document'", "'image'"] {
        insert_ok(
            &db,
            "attachments",
            &attachment_row(next(), 1).set("attachment_kind", kind),
        )
        .await;
    }
    insert_err(
        &db,
        "attachments",
        &attachment_row(next(), 1).set("attachment_kind", "'video'"),
        "attachment_kind",
    )
    .await;
    for status in ["'pending'", "'uploaded'", "'ready'", "'failed'"] {
        insert_ok(
            &db,
            "attachments",
            &attachment_row(next(), 1).set("status", status),
        )
        .await;
    }
    insert_err(
        &db,
        "attachments",
        &attachment_row(next(), 1).set("status", "'weird'"),
        "status",
    )
    .await;
    for secondary in ["'not_attempted'", "'pending'", "'uploaded'", "'failed'"] {
        insert_ok(
            &db,
            "attachments",
            &attachment_row(next(), 1).set("secondary_status", secondary),
        )
        .await;
    }
    insert_err(
        &db,
        "attachments",
        &attachment_row(next(), 1).set("secondary_status", "'bogus'"),
        "secondary_status",
    )
    .await;
    insert_ok(
        &db,
        "attachments",
        &attachment_row(next(), 1).set("secondary_provider_kind", "'anthropic'"),
    )
    .await;
    insert_ok(
        &db,
        "attachments",
        &attachment_row(next(), 1).set("secondary_provider_kind", "NULL"),
    )
    .await;
    insert_err(
        &db,
        "attachments",
        &attachment_row(next(), 1).set("secondary_provider_kind", "'gemini'"),
        "secondary_provider_kind",
    )
    .await;
}

// ── constraints that are deliberately not enforced (ADR-0010) ────────────────

#[tokio::test]
async fn the_cross_column_chat_turn_checks_are_not_enforced_by_the_database() {
    let db = migrated().await;
    for chat in [1u128, 2, 3] {
        insert_ok(&db, "chats", &chat_row(chat)).await;
    }
    // a terminal state without completed_at
    insert_ok(
        &db,
        "chat_turns",
        &turn_row(20, 1, 100).set("completed_at", "NULL"),
    )
    .await;
    // a running turn with completed_at set
    let running_completed = turn_row(21, 2, 101)
        .set("state", "'running'")
        .set("completed_at", TS)
        .set("last_progress_at", TS);
    insert_ok(&db, "chat_turns", &running_completed).await;
    // a running turn without last_progress_at
    let running_idle = turn_row(22, 3, 102)
        .set("state", "'running'")
        .set("completed_at", "NULL")
        .set("last_progress_at", "NULL");
    insert_ok(&db, "chat_turns", &running_idle).await;
}

#[tokio::test]
async fn the_attachment_cleanup_status_value_set_is_not_checked() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    for (n, value) in [
        (400u128, "'pending'"),
        (401, "'done'"),
        (402, "'failed'"),
        (403, "'anything-else'"),
    ] {
        insert_ok(
            &db,
            "attachments",
            &attachment_row(n, 1).set("cleanup_status", value),
        )
        .await;
    }
}

// ── foreign keys and cascades ────────────────────────────────────────────────

#[tokio::test]
async fn child_rows_require_their_chat_and_cascade_with_it() {
    let db = migrated().await;
    insert_err(&db, "messages", &message_row(10, 999), "unknown chat").await;
    insert_err(&db, "chat_turns", &turn_row(20, 999, 100), "unknown chat").await;
    insert_err(&db, "attachments", &attachment_row(30, 999), "unknown chat").await;

    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "messages", &message_row(10, 1)).await;
    insert_ok(&db, "chat_turns", &turn_row(20, 1, 100)).await;
    insert_ok(&db, "attachments", &attachment_row(30, 1)).await;
    insert_ok(&db, "message_reactions", &reaction_row(80, 10, 0xB)).await;

    exec(&db, format!("DELETE FROM chats WHERE id = {}", id(1)))
        .await
        .unwrap();
    for table in ["messages", "chat_turns", "attachments", "message_reactions"] {
        assert_eq!(
            scalar(&db, &format!("SELECT COUNT(*) FROM {table}")).await,
            0,
            "{table} cascades"
        );
    }
}

#[tokio::test]
async fn message_attachments_enforce_that_both_rows_belong_to_the_same_chat() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "chats", &chat_row(2)).await;
    insert_ok(&db, "messages", &message_row(10, 1)).await;
    insert_ok(&db, "attachments", &attachment_row(30, 1)).await;
    insert_ok(&db, "attachments", &attachment_row(31, 2)).await;
    let link = |chat: u128, message: u128, attachment: u128| {
        Row::of(&[
            ("tenant_id", &id(0xA)),
            ("chat_id", &id(chat)),
            ("message_id", &id(message)),
            ("attachment_id", &id(attachment)),
            ("created_at", TS),
        ])
    };

    insert_ok(&db, "message_attachments", &link(1, 10, 30)).await;
    insert_err(
        &db,
        "message_attachments",
        &link(1, 10, 30),
        "composite primary key",
    )
    .await;
    insert_err(
        &db,
        "message_attachments",
        &link(1, 10, 31),
        "attachment from another chat",
    )
    .await;
    insert_err(
        &db,
        "message_attachments",
        &link(2, 10, 31),
        "message from another chat",
    )
    .await;
    insert_err(
        &db,
        "message_attachments",
        &link(1, 999, 30),
        "unknown message",
    )
    .await;
}

#[tokio::test]
async fn message_attachments_cascade_with_the_message_and_with_the_attachment() {
    let db = migrated().await;
    insert_ok(&db, "chats", &chat_row(1)).await;
    insert_ok(&db, "messages", &message_row(10, 1)).await;
    insert_ok(&db, "messages", &message_row(11, 1)).await;
    insert_ok(&db, "attachments", &attachment_row(30, 1)).await;
    let link = |message: u128| {
        Row::of(&[
            ("tenant_id", &id(0xA)),
            ("chat_id", &id(1)),
            ("message_id", &id(message)),
            ("attachment_id", &id(30)),
            ("created_at", TS),
        ])
    };
    insert_ok(&db, "message_attachments", &link(10)).await;
    insert_ok(&db, "message_attachments", &link(11)).await;

    exec(&db, format!("DELETE FROM messages WHERE id = {}", id(10)))
        .await
        .unwrap();
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM message_attachments").await,
        1
    );
    exec(
        &db,
        format!("DELETE FROM attachments WHERE id = {}", id(30)),
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM message_attachments").await,
        0
    );
}

// ── indexes ──────────────────────────────────────────────────────────────────

struct IndexInfo {
    unique: bool,
    partial: bool,
    columns: Vec<String>,
    sql: String,
}

async fn indexes_of(db: &DatabaseConnection, table: &str) -> Vec<IndexInfo> {
    let rows = db
        .query_all_raw(stmt(
            db,
            format!("SELECT name, \"unique\", partial FROM pragma_index_list('{table}')"),
        ))
        .await
        .expect("index list");
    let mut found = Vec::new();
    for row in rows {
        let name: String = row.try_get_by_index(0).unwrap();
        let unique: i64 = row.try_get_by_index(1).unwrap();
        let partial: i64 = row.try_get_by_index(2).unwrap();
        let columns = db
            .query_all_raw(stmt(
                db,
                format!("SELECT name FROM pragma_index_info('{name}') ORDER BY seqno"),
            ))
            .await
            .expect("index info")
            .iter()
            .map(|r| r.try_get_by_index::<String>(0).unwrap())
            .collect();
        let sql = db
            .query_one_raw(stmt(
                db,
                format!("SELECT COALESCE(sql, '') FROM sqlite_master WHERE name = '{name}'"),
            ))
            .await
            .expect("index sql")
            .map(|r| r.try_get_by_index::<String>(0).unwrap())
            .unwrap_or_default();
        found.push(IndexInfo {
            unique: unique == 1,
            partial: partial == 1,
            columns,
            sql: normalize(&sql),
        });
    }
    found
}

fn normalize(sql: &str) -> String {
    sql.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Assert that `table` has an index over exactly `columns` with the given flags and, for a
/// partial index, a predicate containing every fragment.
async fn assert_index(
    db: &DatabaseConnection,
    table: &str,
    columns: &[&str],
    unique: bool,
    partial: Option<&[&str]>,
) {
    let indexes = indexes_of(db, table).await;
    let wanted: Vec<String> = columns.iter().map(|c| (*c).to_owned()).collect();
    let hit = indexes.iter().any(|ix| {
        ix.columns == wanted
            && ix.unique == unique
            && match partial {
                None => !ix.partial,
                Some(fragments) => ix.partial && fragments.iter().all(|f| ix.sql.contains(f)),
            }
    });
    assert!(
        hit,
        "{table}: no {}{} index over {columns:?} (have {:?})",
        if unique { "unique " } else { "" },
        if partial.is_some() { "partial " } else { "" },
        indexes
            .iter()
            .map(|ix| (ix.columns.clone(), ix.unique, ix.partial, ix.sql.clone()))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn chats_have_the_partial_listing_index() {
    let db = migrated().await;
    assert_index(
        &db,
        "chats",
        &["tenant_id", "user_id", "updated_at"],
        false,
        Some(&["deleted_at is null", "desc"]),
    )
    .await;
}

#[tokio::test]
async fn messages_have_their_unique_and_scan_indexes() {
    let db = migrated().await;
    assert_index(
        &db,
        "messages",
        &["chat_id", "request_id", "role"],
        true,
        Some(&["request_id is not null", "deleted_at is null"]),
    )
    .await;
    assert_index(
        &db,
        "messages",
        &["chat_id", "created_at"],
        false,
        Some(&["deleted_at is null"]),
    )
    .await;
    assert_index(&db, "messages", &["id", "chat_id"], true, None).await;
}

#[tokio::test]
async fn chat_turns_have_their_unique_running_and_scan_indexes() {
    let db = migrated().await;
    assert_index(&db, "chat_turns", &["chat_id", "request_id"], true, None).await;
    assert_index(
        &db,
        "chat_turns",
        &["chat_id", "started_at"],
        false,
        Some(&["deleted_at is null", "desc"]),
    )
    .await;
    assert_index(
        &db,
        "chat_turns",
        &["chat_id"],
        true,
        Some(&["state = 'running'", "deleted_at is null"]),
    )
    .await;
    assert_index(
        &db,
        "chat_turns",
        &["last_progress_at"],
        false,
        Some(&["state = 'running'", "deleted_at is null"]),
    )
    .await;
}

#[tokio::test]
async fn attachments_have_their_partial_and_plain_indexes() {
    let db = migrated().await;
    assert_index(
        &db,
        "attachments",
        &["tenant_id", "chat_id"],
        false,
        Some(&["deleted_at is null"]),
    )
    .await;
    assert_index(
        &db,
        "attachments",
        &["cleanup_status"],
        false,
        Some(&["cleanup_status is not null", "deleted_at is null"]),
    )
    .await;
    assert_index(
        &db,
        "attachments",
        &["status", "cleanup_status", "deleted_at", "updated_at"],
        false,
        None,
    )
    .await;
    assert_index(&db, "attachments", &["id", "chat_id"], true, None).await;
}

#[tokio::test]
async fn the_stale_upload_index_has_its_documented_name_and_is_not_partial() {
    let db = migrated().await;
    let sql = text(&db, "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_attachments_stale_upload'").await;
    assert!(
        !normalize(&sql).contains(" where "),
        "must be a plain index: {sql}"
    );
}

#[tokio::test]
async fn the_remaining_tables_have_their_lookup_indexes() {
    let db = migrated().await;
    assert_index(
        &db,
        "message_attachments",
        &["tenant_id", "chat_id"],
        false,
        None,
    )
    .await;
    assert_index(
        &db,
        "message_attachments",
        &["attachment_id", "chat_id"],
        false,
        None,
    )
    .await;
    assert_index(&db, "thread_summaries", &["chat_id"], true, None).await;
    assert_index(
        &db,
        "chat_vector_stores",
        &["tenant_id", "chat_id"],
        true,
        None,
    )
    .await;
    let quota = [
        "tenant_id",
        "user_id",
        "period_type",
        "period_start",
        "bucket",
    ];
    assert_index(&db, "quota_usage", &quota, true, None).await;
    assert_index(&db, "quota_usage", &quota, false, None).await;
    assert_index(
        &db,
        "message_reactions",
        &["message_id", "user_id"],
        true,
        None,
    )
    .await;
}

// ── outbox tables, ordering and reversibility ────────────────────────────────

#[tokio::test]
async fn the_gear_migration_list_includes_the_platform_outbox_migrations() {
    let names: Vec<String> = all_migrations()
        .iter()
        .map(|m| m.name().to_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|n| n == "m001_create_toolkit_outbox_schema"),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("initial_schema")),
        "{names:?}"
    );
    let distinct: BTreeSet<&String> = names.iter().collect();
    assert_eq!(
        distinct.len(),
        names.len(),
        "migration names must be unique: {names:?}"
    );
}

#[tokio::test]
async fn the_full_migration_list_creates_the_nine_tables_and_the_shared_outbox_tables() {
    let dsn = format!(
        "sqlite:file:mc_schema_{}?mode=memory&cache=shared",
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
    .expect("connect");
    run_migrations_for_testing(&db, all_migrations())
        .await
        .expect("run all migrations");
    let raw = Database::connect(&dsn).await.expect("raw connection");
    let tables = table_names(&raw).await;
    for table in NINE_TABLES {
        assert!(tables.contains(table), "missing {table}");
    }
    for table in [
        "toolkit_outbox_body",
        "toolkit_outbox_incoming",
        "toolkit_outbox_dead_letters",
    ] {
        assert!(tables.contains(table), "missing outbox table {table}");
    }
}

#[tokio::test]
async fn the_migration_is_reversible_and_repeatable() {
    let db = Database::connect("sqlite::memory:").await.expect("connect");
    exec(&db, "PRAGMA foreign_keys = ON;").await.unwrap();
    Migrator::up(&db, None).await.expect("up");
    Migrator::down(&db, None).await.expect("down");
    let remaining = table_names(&db).await;
    for table in NINE_TABLES {
        assert!(
            !remaining.contains(table),
            "{table} must be dropped by down"
        );
    }
    Migrator::up(&db, None).await.expect("up again");
    assert!(table_names(&db).await.contains("chats"));
}
